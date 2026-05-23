// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Daemon session manager and socket listeners.
//!
//! [`SessionManager`] is the core daemon component. It binds two Unix domain
//! sockets — one for MCP connections from `catenary bridge` proxies, one for
//! hook connections from `catenary hook` CLI processes — and tracks MCP
//! connections by file descriptor. Hook connections are short-lived
//! (one request-response each).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};

use crate::bridge::EditingGuardrail;
use crate::bridge::HookRouter;
use crate::bridge::McpRouter;
use crate::bridge::is_catenary_tool;
use crate::bridge::session::Session;
use crate::hook::{HookRequest, HookResponseEnvelope, emit_hook_event, hook_outcome_level};
use crate::logging::LoggingServer;
use crate::mcp::{McpServer, ToolHandler};
use crate::source::Source;

/// Returns the MCP socket path for the daemon.
///
/// The path is deterministic: `$XDG_STATE_HOME/catenary/catenary.sock`
/// (or platform equivalent via [`crate::db::state_dir`]).
#[must_use]
pub fn mcp_socket_path() -> PathBuf {
    crate::db::state_dir()
        .join("catenary")
        .join("catenary.sock")
}

/// Returns the hook socket path for the daemon.
///
/// The path is deterministic: `$XDG_STATE_HOME/catenary/catenary-hooks.sock`
/// (or platform equivalent via [`crate::db::state_dir`]).
///
/// Hook CLI processes (`catenary hook pre-tool`, `post-tool`, etc.) connect
/// to this socket instead of discovering a per-session socket. The
/// `session_id` is sent in the hook payload — routing happens daemon-side.
#[must_use]
pub fn hook_socket_path() -> PathBuf {
    crate::db::state_dir()
        .join("catenary")
        .join("catenary-hooks.sock")
}

/// Pre-bound MCP and hook socket listeners.
///
/// Returned by [`bind_daemon_sockets`] for early socket binding in daemon
/// mode. Pass to [`SessionManager::from_listeners`] once the tool handler
/// is ready.
#[cfg(unix)]
pub struct DaemonSockets {
    /// MCP socket listener.
    pub mcp_listener: tokio::net::UnixListener,
    /// Hook socket listener.
    pub hook_listener: tokio::net::UnixListener,
    /// Filesystem path of the MCP socket.
    pub mcp_path: PathBuf,
    /// Filesystem path of the hook socket.
    pub hook_path: PathBuf,
}

/// Binds the daemon's MCP and hook sockets immediately.
///
/// Call this early in daemon startup so that bridge proxies can connect
/// while heavy initialization (config loading, LSP spawning) proceeds.
/// The kernel queues incoming connections until [`SessionManager::accept_loop`]
/// starts processing them.
///
/// # Errors
///
/// Returns an error if directories cannot be created or sockets cannot
/// be bound.
#[cfg(unix)]
pub fn bind_daemon_sockets() -> Result<DaemonSockets> {
    let mcp_path = mcp_socket_path();
    let hook_path = hook_socket_path();

    if let Some(parent) = mcp_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory: {}", parent.display()))?;
    }
    if let Some(parent) = hook_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory: {}", parent.display()))?;
    }

    let mcp_listener = tokio::net::UnixListener::bind(&mcp_path)
        .with_context(|| format!("bind MCP socket: {}", mcp_path.display()))?;
    let hook_listener = tokio::net::UnixListener::bind(&hook_path)
        .with_context(|| format!("bind hook socket: {}", hook_path.display()))?;

    info!(
        source = Source::DaemonLifecycle.as_str(),
        mcp_path = %mcp_path.display(),
        hook_path = %hook_path.display(),
        "daemon sockets bound",
    );

    Ok(DaemonSockets {
        mcp_listener,
        hook_listener,
        mcp_path,
        hook_path,
    })
}

// ── Sandwich correlation ────────────────────────────────────────────

/// Timeout for the correlation window. If no MCP `tools/call` arrives
/// within this duration of a `PreToolUse` hook, the pending entry is
/// discarded and the serialization lock is released.
#[cfg(unix)]
const CORRELATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Entry for a pending correlation: a session awaiting its MCP connection.
///
/// Holds an owned semaphore permit that keeps the serialization lock
/// held. Dropping this entry releases the permit and unblocks the
/// next unclaimed session.
#[cfg(unix)]
struct PendingEntry {
    session_id: String,
    #[allow(dead_code, reason = "compared for timeout staleness")]
    created_at: std::time::Instant,
    /// Owned semaphore permit — dropped when correlation resolves or
    /// times out, releasing the serialization lock.
    permit: tokio::sync::OwnedSemaphorePermit,
}

/// Shared correlation state for sandwich serialization.
///
/// Cloneable for passing to MCP connection callbacks and hook handlers.
/// All fields are `Arc`-wrapped for concurrent access from multiple
/// async tasks and blocking threads.
#[cfg(unix)]
#[derive(Clone)]
struct CorrelationState {
    /// Serialization semaphore (1 permit): only one unclaimed session
    /// can be in the correlation window at a time.
    semaphore: Arc<tokio::sync::Semaphore>,
    /// Pending correlation entry. Set by hook dispatch for `PreToolUse`
    /// on Catenary tools, consumed by the MCP `tools/call` handler.
    pending: Arc<std::sync::Mutex<Option<PendingEntry>>>,
    /// Resolved mappings: fd → `session_id`. Once set, permanent for
    /// the lifetime of the connection.
    connection_sessions: Arc<std::sync::Mutex<HashMap<i32, String>>>,
    /// Reverse lookup: `session_id` → fd. Used for the fast-path
    /// check (skip serialization lock when session already has a
    /// bound connection).
    session_connections: Arc<std::sync::Mutex<HashMap<String, i32>>>,
    /// Per-connection roots refresh flags, keyed by `session_id`.
    /// Created per MCP connection; stored here after correlation.
    /// The hook dispatcher sets this on `PreAgent` to trigger a
    /// `roots/list` poll on the correlated MCP connection.
    roots_refresh_flags: Arc<std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

#[cfg(unix)]
impl CorrelationState {
    fn new() -> Self {
        Self {
            semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            pending: Arc::new(std::sync::Mutex::new(None)),
            connection_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_connections: Arc::new(std::sync::Mutex::new(HashMap::new())),
            roots_refresh_flags: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }
}

/// Per-connection tool handler that dispatches to the correct session
/// after correlation.
///
/// Before correlation, delegates to the shared `ToolHandler` (which
/// uses the daemon's shared `Session`). After correlation, looks up
/// the per-session `Session` from the registry and dispatches through
/// a per-session `McpRouter`.
#[cfg(unix)]
struct CorrelatingHandler {
    /// Shared handler (pre-correlation fallback).
    inner: Arc<dyn ToolHandler>,
    /// Session registry (shared with hook dispatch).
    sessions: Arc<std::sync::Mutex<HashMap<String, SessionEntry>>>,
    /// Correlation state for fd → `session_id` lookup.
    correlation: CorrelationState,
    /// This connection's file descriptor.
    fd: i32,
    /// Per-connection roots refresh flag, wired to the `McpServer` via
    /// `on_roots_refresh`. Each `call_tool` bridges the per-session
    /// flag to this one so the MCP run loop sees it.
    roots_refresh: Arc<AtomicBool>,
    /// Cached per-session handler and session (set on first
    /// post-correlation call).
    cached: std::sync::Mutex<Option<(McpRouter, Arc<Session>)>>,
}

#[cfg(unix)]
impl CorrelatingHandler {
    /// Bridges the per-session `roots_refresh_requested` flag to the
    /// per-connection flag. Called on every `call_tool` so the MCP run
    /// loop's flag check (which happens between dispatches) sees the
    /// value set by the `PreAgent` hook.
    fn bridge_roots_refresh(&self, session: &Session) {
        if session
            .roots_refresh_requested
            .swap(false, Ordering::AcqRel)
        {
            self.roots_refresh.store(true, Ordering::Release);
        }
    }
}

#[cfg(unix)]
impl ToolHandler for CorrelatingHandler {
    fn list_tools(&self) -> Vec<crate::mcp::Tool> {
        self.inner.list_tools()
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        parent_id: Option<String>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<crate::mcp::CallToolResult> {
        // Fast path: already have a cached per-session handler.
        if let Some((handler, session)) = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            self.bridge_roots_refresh(session);
            return handler.call_tool(name, arguments, parent_id, cancel);
        }

        // Check if this connection has been correlated.
        let session_id = self
            .correlation
            .connection_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.fd)
            .cloned();

        if let Some(sid) = session_id {
            // Look up the per-session Session and cache a McpRouter.
            let session = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&sid)
                .map(|entry| entry.session.clone());

            if let Some(session) = session {
                self.bridge_roots_refresh(&session);
                let router = McpRouter::new(session.clone());
                let result = router.call_tool(name, arguments, parent_id, cancel);
                *self
                    .cached
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((router, session));
                return result;
            }
        }

        // Not yet correlated — use the shared handler.
        self.inner.call_tool(name, arguments, parent_id, cancel)
    }
}

// ── Session registry ───────────────────────────────────────────────

/// Per-session state: the session's own `Session` and its `HookRouter`.
///
/// The `session` field is used by MCP connection correlation to bind
/// MCP connections to the correct session's tool servers.
#[cfg(unix)]
struct SessionEntry {
    session: Arc<Session>,
    router: Arc<HookRouter>,
}

/// Shared context for session-aware hook dispatch.
///
/// When set on [`SessionManager`], hook connections are routed to
/// per-`session_id` [`Session`] + [`HookRouter`] pairs. Each session
/// has independent editing state and turn counter. Heavy resources
/// (`LspClientManager`, config, logging) are shared via `Arc` from the
/// daemon's primary session. When absent, hooks receive passthrough
/// responses (allow everything).
#[cfg(unix)]
#[derive(Clone)]
struct HookDispatchContext {
    /// Per-`session_id` session entries. Each entry has its own
    /// `Session` (per-session state) and `HookRouter` (turn counter,
    /// debounce).
    sessions: Arc<std::sync::Mutex<HashMap<String, SessionEntry>>>,
    /// Daemon's primary session — used as the template for creating
    /// per-session sessions via [`Session::new_for_daemon`].
    primary: Arc<Session>,
    /// Shared database connection for `HookRouter` DB writes.
    conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    /// Logging server for correlation ID minting and sink access.
    logging: LoggingServer,
    /// Root tracker for refcount-aware root management across sessions.
    root_tracker: Option<RootTracker>,
    /// Cross-session per-root editing guardrail. Shared with all
    /// per-session `Session` instances to prevent concurrent editing
    /// in the same workspace root.
    editing_guardrail: Arc<EditingGuardrail>,
    /// Serialization semaphore (1 permit): only one session can be
    /// in the `done_editing` handoff window at a time.
    handoff_semaphore: Arc<tokio::sync::Semaphore>,
    /// Handoff slot: file list + owned permit deposited by
    /// `PreToolUse`, consumed by the `done_editing` CLI command.
    handoff_slot: Arc<std::sync::Mutex<Option<HandoffContext>>>,
}

/// Handoff context deposited by `pre-tool/done-editing-prepare`
/// and consumed by `done-editing/run`.
///
/// Dropping this struct drops the owned semaphore permit, releasing
/// the handoff lock.
struct HandoffContext {
    /// Accumulated files from the editing session.
    files: Vec<PathBuf>,
    /// Owned semaphore permit — dropped when the `HandoffContext`
    /// is dropped (slot consumed or timeout), releasing the lock.
    /// Never read directly; held purely for RAII drop semantics.
    #[allow(dead_code, reason = "RAII guard — held for drop, not read")]
    permit: tokio::sync::OwnedSemaphorePermit,
}

/// Spawns a background task that clears the handoff slot after 5 seconds.
///
/// Handles the case where the CLI command never connects (e.g., the host
/// kills the subprocess between `PreToolUse` and command execution).
/// Dropping the `HandoffContext` drops the owned permit, releasing the
/// semaphore.
fn spawn_handoff_timeout(slot: Arc<std::sync::Mutex<Option<HandoffContext>>>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut s = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.is_some() {
            // Dropping the HandoffContext drops the owned permit.
            *s = None;
            warn!(
                source = Source::DaemonDispatch.as_str(),
                "done_editing handoff timeout — discarding file list",
            );
        }
    });
}

/// Tracks per-contributor workspace root sets for reference counting.
///
/// Each MCP connection and CLI command contributes a set of roots
/// keyed by a contributor string. The global root set (union of all
/// contributors) is synced to the shared [`crate::lsp::LspClientManager`]
/// after each mutation. When a contributor is removed (MCP disconnect),
/// its roots leave the union — roots that no other contributor provides
/// are removed from LSP servers via `didChangeWorkspaceFolders`.
///
/// Contributor keys:
/// - `"mcp:{fd}"` — roots from MCP `roots/list` for a connection
/// - `"hook"` — roots from `catenary add-root` CLI commands
#[cfg(unix)]
#[derive(Clone)]
struct RootTracker {
    /// Per-contributor root sets. The global root set is the union of
    /// all values.
    contributors: Arc<std::sync::Mutex<HashMap<String, HashSet<PathBuf>>>>,
}

#[cfg(unix)]
impl RootTracker {
    fn new() -> Self {
        Self {
            contributors: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Replaces a contributor's root set.
    fn set_roots(&self, contributor: &str, roots: Vec<PathBuf>) {
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(contributor.to_string(), roots.into_iter().collect());
    }

    /// Adds roots to a contributor's set (does not remove existing ones).
    fn add_roots(&self, contributor: &str, roots: &[PathBuf]) {
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(contributor.to_string())
            .or_default()
            .extend(roots.iter().cloned());
    }

    /// Removes a contributor entirely.
    fn remove_contributor(&self, contributor: &str) {
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(contributor);
    }

    /// Removes a single root from a contributor's set.
    ///
    /// Returns `true` if the root was present and removed, `false` if
    /// the contributor or root was not found.
    #[allow(
        clippy::option_if_let_else,
        reason = "map_or causes double-borrow on the Mutex guard"
    )]
    fn remove_root(&self, contributor: &str, root: &Path) -> bool {
        let mut map = self
            .contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(roots) = map.get_mut(contributor) {
            let removed = roots.remove(root);
            if roots.is_empty() {
                map.remove(contributor);
            }
            removed
        } else {
            false
        }
    }

    /// Returns the union of all contributors' root sets.
    fn global_roots(&self) -> Vec<PathBuf> {
        let map = self
            .contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut all = HashSet::new();
        for roots in map.values() {
            all.extend(roots.iter().cloned());
        }
        drop(map);
        all.into_iter().collect()
    }

    /// Returns the number of contributors that include the given root.
    #[cfg(test)]
    fn refcount(&self, root: &Path) -> usize {
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|roots| roots.contains(root))
            .count()
    }
}

/// Core daemon component that manages MCP and hook socket connections.
///
/// Binds two Unix domain sockets: one for MCP connections from `catenary
/// bridge` proxies, one for hook connections from `catenary hook` CLI
/// processes. Each MCP connection spawns a per-connection async task
/// with a full domain stack (`McpServer`/`ToolServer`), dispatching to
/// the shared `LspClientManager` via the tool handler. Hook connections
/// are routed to per-`session_id` [`HookRouter`] instances when a shared
/// [`Session`] is configured (daemon mode), or receive passthrough
/// responses (test mode).
#[cfg(unix)]
pub struct SessionManager {
    mcp_listener: tokio::net::UnixListener,
    hook_listener: tokio::net::UnixListener,
    mcp_socket_path: PathBuf,
    hook_socket_path: PathBuf,
    handler: Arc<dyn ToolHandler>,
    logging: LoggingServer,
    connection_count: Arc<AtomicUsize>,
    /// Session-aware hook dispatch context. `None` in tests that don't
    /// exercise hook routing (passthrough mode).
    hook_ctx: Option<HookDispatchContext>,
    /// Shared LSP infrastructure for MCP lifecycle callbacks
    /// (`on_roots_changed`). `None` in transport-only tests.
    lsp: Option<Arc<crate::lsp::LspClientManager>>,
    /// Root tracker for refcount-aware root management across sessions.
    /// `None` in transport-only tests; set by [`Self::with_session`].
    root_tracker: Option<RootTracker>,
    /// Shared DB connection for `on_client_info`. `None` in
    /// transport-only tests.
    db_conn: Option<Arc<std::sync::Mutex<rusqlite::Connection>>>,
    /// Sandwich correlation state for binding MCP connections to sessions.
    correlation: CorrelationState,
    shutdown: CancellationToken,
    disconnect: Arc<tokio::sync::Notify>,
}

#[cfg(unix)]
impl SessionManager {
    /// Binds the MCP and hook sockets at the default paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory cannot be created or
    /// either socket cannot be bound (e.g., another daemon is already
    /// running).
    pub fn bind(handler: Arc<dyn ToolHandler>, logging: LoggingServer) -> Result<Self> {
        Self::bind_at(&mcp_socket_path(), &hook_socket_path(), handler, logging)
    }

    /// Creates a `SessionManager` from pre-bound sockets.
    ///
    /// Consumes the [`DaemonSockets`] returned by [`bind_daemon_sockets`],
    /// transferring socket ownership. Used in daemon mode where sockets
    /// are bound before heavy initialization so bridges can connect
    /// immediately. [`SessionManager::drop`] cleans up the socket files.
    #[must_use]
    pub fn from_sockets(
        sockets: DaemonSockets,
        handler: Arc<dyn ToolHandler>,
        logging: LoggingServer,
    ) -> Self {
        Self {
            mcp_listener: sockets.mcp_listener,
            hook_listener: sockets.hook_listener,
            mcp_socket_path: sockets.mcp_path,
            hook_socket_path: sockets.hook_path,
            handler,
            logging,
            connection_count: Arc::new(AtomicUsize::new(0)),
            hook_ctx: None,
            lsp: None,
            root_tracker: None,
            db_conn: None,
            correlation: CorrelationState::new(),
            shutdown: CancellationToken::new(),
            disconnect: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Binds the MCP and hook sockets at explicit paths.
    ///
    /// Used by tests to isolate socket files in tempdirs.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directories cannot be created or
    /// either socket cannot be bound.
    pub fn bind_at(
        mcp_path: &Path,
        hook_path: &Path,
        handler: Arc<dyn ToolHandler>,
        logging: LoggingServer,
    ) -> Result<Self> {
        if let Some(parent) = mcp_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory: {}", parent.display()))?;
        }
        if let Some(parent) = hook_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory: {}", parent.display()))?;
        }

        let mcp_listener = tokio::net::UnixListener::bind(mcp_path)
            .with_context(|| format!("bind MCP socket: {}", mcp_path.display()))?;
        let hook_listener = tokio::net::UnixListener::bind(hook_path)
            .with_context(|| format!("bind hook socket: {}", hook_path.display()))?;

        info!(
            source = Source::DaemonLifecycle.as_str(),
            mcp_path = %mcp_path.display(),
            hook_path = %hook_path.display(),
            "daemon started",
        );

        Ok(Self {
            mcp_listener,
            hook_listener,
            mcp_socket_path: mcp_path.to_path_buf(),
            hook_socket_path: hook_path.to_path_buf(),
            handler,
            logging,
            connection_count: Arc::new(AtomicUsize::new(0)),
            hook_ctx: None,
            lsp: None,
            root_tracker: None,
            db_conn: None,
            correlation: CorrelationState::new(),
            shutdown: CancellationToken::new(),
            disconnect: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Accepts incoming MCP and hook connections in a loop.
    ///
    /// Each MCP connection spawns a per-connection async task with a full
    /// MCP stack (`McpServer` backed by the shared tool handler). The
    /// task runs in a tracing span tagged with `mcp_fd` for log
    /// correlation. Hook connections are short-lived and handled in
    /// spawned tasks with passthrough responses.
    ///
    /// Returns `Ok(())` when the daemon should shut down. Three triggers:
    /// - Last MCP client disconnected (disconnect notify, count == 0)
    /// - `catenary stop` received on the hook socket (shutdown token)
    /// - External signal cancelled the shutdown token
    ///
    /// On exit, socket files are removed so new bridges start a fresh
    /// daemon instead of connecting to one that is shutting down.
    ///
    /// # Errors
    ///
    /// Returns an error if either listener encounters a fatal I/O error.
    pub async fn accept_loop(&self) -> Result<()> {
        use std::os::fd::AsRawFd;

        loop {
            tokio::select! {
                result = self.mcp_listener.accept() => {
                    let (stream, _addr) = result.context("accept MCP connection")?;
                    let fd = stream.as_raw_fd();
                    self.handle_mcp_connection(stream, fd);
                }
                result = self.hook_listener.accept() => {
                    let (stream, _addr) = result.context("accept hook connection")?;
                    let shutdown = self.shutdown.clone();
                    if let Some(ctx) = &self.hook_ctx {
                        let ctx = ctx.clone();
                        let correlation = self.correlation.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_hook_dispatch(stream, ctx, correlation, shutdown).await {
                                debug!(
                                    source = Source::DaemonDispatch.as_str(),
                                    "hook connection error: {e}",
                                );
                            }
                        });
                    } else {
                        tokio::spawn(async move {
                            if let Err(e) = handle_hook_connection(stream, shutdown).await {
                                debug!(
                                    source = Source::DaemonDispatch.as_str(),
                                    "hook connection error: {e}",
                                );
                            }
                        });
                    }
                }
                () = self.shutdown.cancelled() => {
                    self.remove_sockets();
                    return Ok(());
                }
                () = self.disconnect.notified() => {
                    if self.connection_count.load(Ordering::Acquire) == 0 {
                        info!(
                            source = Source::DaemonLifecycle.as_str(),
                            "last client disconnected",
                        );
                        self.remove_sockets();
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Returns the shutdown token for this daemon.
    ///
    /// Cancel this token to initiate daemon shutdown. The
    /// [`accept_loop`](Self::accept_loop) removes socket files and
    /// returns `Ok(())` when the token is cancelled.
    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Removes socket files so new bridges start a fresh daemon.
    fn remove_sockets(&self) {
        let _ = std::fs::remove_file(&self.mcp_socket_path);
        let _ = std::fs::remove_file(&self.hook_socket_path);
    }

    /// Spawns a per-connection MCP task.
    ///
    /// Converts the tokio `UnixStream` to a `std::os::unix::net::UnixStream`
    /// (since `McpServer` uses synchronous I/O), clones it for
    /// read/write halves, and runs the MCP message loop in a blocking task.
    /// A [`ConnectionGuard`] decrements the connection count on any exit
    /// path and notifies the accept loop, which checks whether the daemon
    /// should shut down.
    #[allow(clippy::too_many_lines, reason = "sequential connection setup steps")]
    fn handle_mcp_connection(&self, stream: tokio::net::UnixStream, fd: i32) {
        let roots_refresh = Arc::new(AtomicBool::new(false));
        let handler: Arc<dyn ToolHandler> = if let Some(ctx) = &self.hook_ctx {
            Arc::new(CorrelatingHandler {
                inner: self.handler.clone(),
                sessions: ctx.sessions.clone(),
                correlation: self.correlation.clone(),
                fd,
                roots_refresh: roots_refresh.clone(),
                cached: std::sync::Mutex::new(None),
            })
        } else {
            self.handler.clone()
        };
        let sessions_for_callback = self.hook_ctx.as_ref().map(|ctx| ctx.sessions.clone());
        let logging = self.logging.clone();
        let count = Arc::clone(&self.connection_count);
        let disconnect = Arc::clone(&self.disconnect);
        let correlation = self.correlation.clone();
        let lsp = self.lsp.clone();
        let root_tracker = self.root_tracker.clone();
        let editing_guardrail = self
            .hook_ctx
            .as_ref()
            .map(|ctx| ctx.editing_guardrail.clone());
        let notification_router = self
            .hook_ctx
            .as_ref()
            .map(|ctx| ctx.primary.notification_router.clone());
        let db_conn = self.db_conn.clone();

        count.fetch_add(1, Ordering::Relaxed);

        tokio::spawn(async move {
            // session_id starts Empty — filled by sandwich correlation on
            // first tools/call. Events before correlation have no session
            // routing: warn!/error! won't reach the agent via systemMessage.
            // This is fine: pre-correlation errors are fatal (connection
            // dies, bridge reports via stderr) or protocol-level (info/debug).
            // Do not add warn!() to the init path expecting agent delivery.
            let span = tracing::info_span!(
                "mcp_connection",
                mcp_fd = fd,
                session_id = tracing::field::Empty,
            );
            let span_for_blocking = span.clone();
            async {
                let _guard = ConnectionGuard { count, disconnect };

                info!(
                    source = Source::DaemonDispatch.as_str(),
                    "MCP connection accepted",
                );

                let std_stream = match stream.into_std() {
                    Ok(s) => {
                        // into_std() returns a non-blocking stream (tokio
                        // default). McpServer uses blocking I/O — switch
                        // to blocking mode before handing off.
                        if let Err(e) = s.set_nonblocking(false) {
                            error!(
                                source = Source::DaemonDispatch.as_str(),
                                "failed to set stream to blocking: {e}",
                            );
                            return;
                        }
                        s
                    }
                    Err(e) => {
                        error!(
                            source = Source::DaemonDispatch.as_str(),
                            "failed to convert socket to std: {e}",
                        );
                        return;
                    }
                };
                let reader = match std_stream.try_clone() {
                    Ok(r) => r,
                    Err(e) => {
                        error!(
                            source = Source::DaemonDispatch.as_str(),
                            "failed to clone socket for reader: {e}",
                        );
                        return;
                    }
                };
                let writer = std_stream;

                // Clone shared state for post-disconnect cleanup
                // (originals move into spawn_blocking).
                let tracker_cleanup = root_tracker.clone();
                let correlation_cleanup = correlation.clone();
                let sessions_cleanup = sessions_for_callback.clone();
                let lsp_cleanup = lsp.clone();
                let guardrail_cleanup = editing_guardrail.clone();
                let notification_router_cleanup = notification_router.clone();

                let result = tokio::task::spawn_blocking(move || {
                    let _entered = span_for_blocking.enter();

                    let corr = correlation;
                    let corr_for_bridge = corr.clone();
                    let resolved = AtomicBool::new(false);
                    let span_ref = tracing::Span::current();
                    let roots_refresh_for_corr = roots_refresh.clone();

                    let mut mcp = McpServer::new(handler, logging)
                        .on_roots_refresh(roots_refresh)
                        .on_tools_call(Box::new(move || {
                            if resolved.load(Ordering::Relaxed) {
                                return;
                            }
                            resolve_correlation(
                                fd,
                                &corr,
                                &roots_refresh_for_corr,
                                &resolved,
                                &span_ref,
                            );
                            if !resolved.load(Ordering::Relaxed) {
                                return;
                            }
                            // Correlation just resolved — bridge the per-session
                            // roots_refresh flag to the per-connection flag.
                            let Some(ref sessions) = sessions_for_callback else {
                                return;
                            };
                            let session_id = corr_for_bridge
                                .connection_sessions
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(&fd)
                                .cloned();
                            let Some(sid) = session_id else {
                                return;
                            };
                            let refreshed = sessions
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(&sid)
                                .is_some_and(|entry| {
                                    entry
                                        .session
                                        .roots_refresh_requested
                                        .swap(false, Ordering::AcqRel)
                                });
                            if refreshed {
                                roots_refresh_for_corr.store(true, Ordering::Release);
                            }
                        }));

                    // Wire lifecycle callbacks when the shared LSP
                    // infrastructure is available (daemon mode). When a
                    // root tracker is configured, root changes go through
                    // refcounting so multiple sessions can share roots
                    // without clobbering each other.
                    match (root_tracker, lsp) {
                        (Some(tracker), Some(cm)) => {
                            let mcp_key = format!("mcp:{fd}");
                            mcp = mcp.on_roots_changed(Box::new(move |roots| {
                                let paths = parse_root_uris(&roots);
                                tracker.set_roots(&mcp_key, paths);
                                let global = tracker.global_roots();
                                tokio::runtime::Handle::current()
                                    .block_on(cm.sync_roots(global))?;
                                Ok(())
                            }));
                        }
                        (None, Some(cm)) => {
                            mcp = mcp.on_roots_changed(Box::new(move |roots| {
                                let paths = parse_root_uris(&roots);
                                tokio::runtime::Handle::current().block_on(cm.sync_roots(paths))?;
                                Ok(())
                            }));
                        }
                        _ => {}
                    }

                    if let Some(conn) = db_conn {
                        mcp = mcp.on_client_info(Box::new(move |name: &str, version: &str| {
                            if let Ok(c) = conn.lock() {
                                let _ = c.execute(
                                    "UPDATE sessions SET client_name = ?1, \
                                     client_version = ?2 WHERE id = 'daemon'",
                                    rusqlite::params![name, version],
                                );
                            }
                        }));
                    }

                    mcp.run(reader, writer)
                })
                .await;

                match result {
                    Ok(Ok(())) => info!(
                        source = Source::DaemonDispatch.as_str(),
                        "MCP connection closed",
                    ),
                    Ok(Err(e)) => error!(
                        source = Source::DaemonDispatch.as_str(),
                        "MCP connection error: {e}",
                    ),
                    Err(e) => error!(
                        source = Source::DaemonDispatch.as_str(),
                        "MCP task panicked: {e}",
                    ),
                }

                // ── Disconnect cleanup ────────────────────────────
                //
                // Two paths clean up session state:
                //
                // 1. **MCP disconnect** (here) — always fires (kernel
                //    delivers socket EOF). Handles fd-scoped state
                //    (MCP roots, correlation mappings) and, for
                //    correlated sessions, also cleans up session-scoped
                //    state (session registry) as a crash-safety
                //    fallback.
                //
                // 2. **SessionEnd hook** (`handle_hook_dispatch`) —
                //    best-effort (host CLI may be killed before the
                //    hook fires). Handles session-scoped state only.
                //    For never-correlated sessions, this is the
                //    primary cleanup path.
                //
                // The session registry cleanup overlaps intentionally:
                // if SessionEnd ran first, the removal here is a no-op.
                // If the host was killed and SessionEnd never fired,
                // this path catches the leak.
                if let Some(ref tracker) = tracker_cleanup {
                    let mcp_key = format!("mcp:{fd}");
                    tracker.remove_contributor(&mcp_key);

                    // Look up and remove fd → session_id mapping.
                    let session_id = correlation_cleanup
                        .connection_sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&fd);

                    if let Some(ref sid) = session_id {
                        if let Some(ref sessions) = sessions_cleanup {
                            sessions
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(sid.as_str());
                        }

                        // Release any editing guardrail locks held by
                        // this session (crash-safety: prevents stuck
                        // locks if the agent dies mid-edit).
                        if let Some(ref guardrail) = guardrail_cleanup {
                            guardrail.release_all(sid);
                        }

                        // Remove session from notification router
                        // (idempotent if SessionEnd already ran).
                        if let Some(ref router) = notification_router_cleanup {
                            router.remove_session(sid);
                        }

                        // Clean up correlation state.
                        correlation_cleanup
                            .session_connections
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(sid.as_str());
                        correlation_cleanup
                            .roots_refresh_flags
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(sid.as_str());

                        info!(
                            source = Source::DaemonDispatch.as_str(),
                            session_id = sid.as_str(),
                            "session cleaned up on disconnect",
                        );
                    }

                    // Sync the reduced root set.
                    if let Some(ref cm) = lsp_cleanup {
                        let global = tracker.global_roots();
                        if let Err(e) = cm.sync_roots(global).await {
                            debug!(
                                source = Source::DaemonDispatch.as_str(),
                                "root sync after disconnect failed: {e}",
                            );
                        }
                    }
                }
            }
            .instrument(span)
            .await;
        });
    }

    /// Returns the number of active MCP connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::Relaxed)
    }

    /// Returns the MCP socket path this manager is bound to.
    #[must_use]
    pub fn mcp_path(&self) -> &Path {
        &self.mcp_socket_path
    }

    /// Returns the hook socket path this manager is bound to.
    #[must_use]
    pub fn hook_path(&self) -> &Path {
        &self.hook_socket_path
    }

    /// Enables session-aware hook dispatch.
    ///
    /// Once set, hook connections create per-`session_id` [`Session`]
    /// instances (via [`Session::new_for_daemon`]) with independent
    /// per-session state. Heavy resources are shared from the primary
    /// session. Without this, hooks receive passthrough responses (test
    /// mode).
    #[must_use]
    pub fn with_session(
        mut self,
        session: Arc<Session>,
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    ) -> Self {
        self.lsp = Some(session.lsp_client_manager().clone());
        self.db_conn = Some(conn.clone());
        let root_tracker = RootTracker::new();
        self.root_tracker = Some(root_tracker.clone());
        self.hook_ctx = Some(HookDispatchContext {
            sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            primary: session,
            conn,
            logging: self.logging.clone(),
            root_tracker: Some(root_tracker),
            editing_guardrail: Arc::new(EditingGuardrail::new()),
            handoff_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            handoff_slot: Arc::new(std::sync::Mutex::new(None)),
        });
        self
    }

    /// Returns the number of active sessions in the registry.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.hook_ctx.as_ref().map_or(0, |ctx| {
            ctx.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        })
    }

    /// Returns the `session_id` bound to an MCP connection, if any.
    #[must_use]
    pub fn session_for_fd(&self, fd: i32) -> Option<String> {
        self.correlation
            .connection_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&fd)
            .cloned()
    }

    /// Returns `true` if the given session has a bound MCP connection.
    #[must_use]
    pub fn session_has_connection(&self, session_id: &str) -> bool {
        self.correlation
            .session_connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(session_id)
    }

    /// Returns `true` if a pending correlation entry is waiting for
    /// an MCP `tools/call`.
    #[must_use]
    pub fn has_pending_correlation(&self) -> bool {
        self.correlation
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

/// RAII guard that decrements the connection count on drop.
///
/// Always notifies the accept loop after decrementing. The accept loop
/// checks the count atomically and decides whether to shut down — this
/// keeps the shutdown decision synchronous on the accept loop's task,
/// eliminating the race between a new connection arriving and the
/// shutdown firing.
#[cfg(unix)]
struct ConnectionGuard {
    count: Arc<AtomicUsize>,
    disconnect: Arc<tokio::sync::Notify>,
}

#[cfg(unix)]
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
        self.disconnect.notify_one();
    }
}

/// Resolves sandwich correlation for an MCP connection.
///
/// Called from the `on_tools_call` callback on each `tools/call` dispatch.
/// If a pending correlation entry exists (from a preceding `PreToolUse` hook),
/// binds this connection's fd to the pending session permanently.
#[cfg(unix)]
fn resolve_correlation(
    fd: i32,
    corr: &CorrelationState,
    roots_refresh: &Arc<AtomicBool>,
    resolved: &AtomicBool,
    span: &tracing::Span,
) {
    // Fast path: already correlated.
    if corr
        .connection_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&fd)
    {
        resolved.store(true, Ordering::Relaxed);
        return;
    }

    // Check pending correlation entry.
    let mut pending = corr
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(PendingEntry {
        session_id,
        permit: _permit,
        ..
    }) = pending.take()
    {
        info!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            mcp_fd = fd,
            "correlation resolved: MCP connection bound to session",
        );

        corr.connection_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(fd, session_id.clone());
        corr.session_connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone(), fd);
        corr.roots_refresh_flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone(), roots_refresh.clone());

        // Enrich the tracing span so subsequent events carry session_id.
        span.record("session_id", &session_id);

        resolved.store(true, Ordering::Relaxed);
        // `permit` drops here → semaphore released
    }
}

/// Returns `true` if the given `session_id` already has a bound MCP
/// connection (fast-path check for skipping the serialization lock).
#[cfg(unix)]
fn is_session_correlated(corr: &CorrelationState, session_id: &str) -> bool {
    corr.session_connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(session_id)
}

/// Returns `true` if this `PreToolUse` hook should trigger correlation.
///
/// Only `PreToolUse` for Catenary MCP tools (grep, glob, `start_editing`,
/// `done_editing`) trigger correlation. Non-Catenary tool hooks (Edit,
/// Bash) are processed entirely on session state without needing the
/// MCP connection mapping.
#[cfg(unix)]
fn is_correlation_trigger(method: &str, raw: &serde_json::Value) -> bool {
    if !method.starts_with("pre-tool") {
        return false;
    }
    let tool_name = raw.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    is_catenary_tool(tool_name, "grep")
        || is_catenary_tool(tool_name, "glob")
        || is_catenary_tool(tool_name, "start_editing")
        || is_catenary_tool(tool_name, "done_editing")
}

/// Spawns a background task that clears a stale pending correlation
/// after [`CORRELATION_TIMEOUT`].
#[cfg(unix)]
fn spawn_correlation_timeout(
    pending: Arc<std::sync::Mutex<Option<PendingEntry>>>,
    session_id: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(CORRELATION_TIMEOUT).await;
        let mut guard = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.as_ref()
            && entry.session_id == session_id
        {
            warn!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                "correlation timeout: no MCP tool call within 30s \
                 of `PreToolUse` — session will re-correlate on next \
                 Catenary tool call",
            );
            *guard = None;
        }
    });
}

/// Extracts canonical file paths from MCP root URIs.
///
/// Filters out non-`file://` URIs and roots that fail to canonicalize.
#[cfg(unix)]
fn parse_root_uris(roots: &[crate::mcp::Root]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| {
            root.uri.strip_prefix("file://").and_then(|p| {
                let path = PathBuf::from(p);
                match path.canonicalize() {
                    Ok(canonical) => Some(canonical),
                    Err(e) => {
                        warn!(
                            source = Source::ConfigValidation.as_str(),
                            "Skipping root {p}: {e}",
                        );
                        None
                    }
                }
            })
        })
        .collect()
}

/// Handles a single hook connection.
///
/// Reads the JSON request, logs the method for visibility, and sends an
/// empty response (which means "allow" in the hook protocol). Recognizes
/// the `"tool/shutdown"` method from `catenary stop` and cancels the daemon
/// shutdown token. Used when no shared session is configured (test mode).
#[cfg(unix)]
async fn handle_hook_connection(
    stream: tokio::net::UnixStream,
    shutdown: CancellationToken,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader
        .read_line(&mut line)
        .await
        .context("read hook request")?;

    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(line.trim()) {
        let method = raw
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if method == "tool/shutdown" {
            info!(
                source = Source::DaemonLifecycle.as_str(),
                "shutdown requested via stop command",
            );
            writer.write_all(b"{\"status\":\"ok\"}\n").await?;
            writer.shutdown().await?;
            shutdown.cancel();
            return Ok(());
        }

        info!(
            source = Source::DaemonDispatch.as_str(),
            method, "hook request (passthrough)",
        );
    }

    // Empty response = "allow" for all hook types.
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;

    Ok(())
}

/// Looks up or creates a per-session [`Session`] + [`HookRouter`] pair.
///
/// Each `session_id` gets its own `Session` (via
/// [`Session::new_for_daemon`]) with independent editing state and
/// notification queue. The `HookRouter` wraps the per-session `Session`
/// with its own turn counter and debounce state.
///
/// Registers the session with the shared [`crate::logging::notification_router::NotificationRouter`]
/// so `warn!()` / `error!()` events carrying this `session_id` in
/// their span context route to this session's notification queue.
#[cfg(unix)]
fn get_or_create_router(ctx: &HookDispatchContext, session_id: &str) -> Arc<HookRouter> {
    ctx.sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(session_id.to_string())
        .or_insert_with(|| {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id, "creating session",
            );
            let session_id_arc: Arc<str> = session_id.into();
            let session = Arc::new(Session::new_for_daemon(
                &ctx.primary,
                session_id_arc,
                Some(ctx.editing_guardrail.clone()),
            ));

            // Register session with the notification router so
            // events carrying this session_id route to its queue.
            session.notification_router.register_session(session_id);

            let router = Arc::new(HookRouter::new(
                session.clone(),
                ctx.conn.clone(),
                session.instance_id.clone(),
                session_id.to_string(),
            ));
            SessionEntry { session, router }
        })
        .router
        .clone()
}

/// Handles a single hook connection with session-aware dispatch.
///
/// Reads the JSON request, extracts `session_id` for routing, looks up
/// (or creates) the per-session [`HookRouter`], dispatches the request,
/// logs the protocol pair, and writes the response. For `PreToolUse`
/// hooks on Catenary tools, enters the sandwich correlation path to bind
/// the next MCP `tools/call` to this session.
#[cfg(unix)]
#[allow(clippy::too_many_lines, reason = "sequential protocol steps")]
async fn handle_hook_dispatch(
    stream: tokio::net::UnixStream,
    ctx: HookDispatchContext,
    correlation: CorrelationState,
    shutdown: CancellationToken,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader
        .read_line(&mut line)
        .await
        .context("read hook request")?;

    let raw: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| anyhow!("Invalid hook request: {e}"))?;
    let method = raw
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Handle shutdown from `catenary stop`.
    if method == "tool/shutdown" {
        info!(
            source = Source::DaemonLifecycle.as_str(),
            "shutdown requested via stop command",
        );
        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        shutdown.cancel();
        return Ok(());
    }

    // Extract session_id for routing. Falls back to "default" for hooks
    // that don't carry a session_id (backward compatibility).
    let session_id = raw
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    // ── Sandwich correlation ───────────────────────────────────────
    // For PreToolUse on Catenary tools: if this session doesn't have
    // a bound MCP connection yet, acquire the serialization lock and
    // record the pending entry. The next MCP tools/call on an
    // unclaimed connection will resolve the correlation.
    if is_correlation_trigger(&method, &raw) && !is_session_correlated(&correlation, &session_id) {
        let permit = correlation
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("correlation semaphore closed"))?;

        *correlation
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PendingEntry {
            session_id: session_id.clone(),
            created_at: std::time::Instant::now(),
            permit,
        });

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            "correlation pending: awaiting MCP tools/call",
        );

        spawn_correlation_timeout(correlation.pending.clone(), session_id.clone());
    }

    // ── Session-end cleanup ───────────────────────────────────────
    //
    // Best-effort counterpart to the MCP disconnect cleanup. Fires
    // when the host CLI sends a SessionEnd hook (exit, /clear,
    // resume, logout). Handles session-scoped state that the
    // disconnect path can't reach for never-correlated sessions
    // (no fd → session_id mapping exists). For correlated sessions,
    // this overlaps with the disconnect path — the removals are
    // idempotent.
    //
    // Short-circuits before get_or_create_router to avoid creating
    // a new session just to immediately clean it up.
    if method == "session-end/cleanup" {
        let id = ctx.logging.next_id();

        // Release editing guardrail locks (idempotent if MCP
        // disconnect already ran).
        ctx.editing_guardrail.release_all(&session_id);

        // Remove session from notification router (idempotent if
        // MCP disconnect already ran).
        ctx.primary.notification_router.remove_session(&session_id);

        if let Some(ref tracker) = ctx.root_tracker {
            // Remove the session from the registry. For correlated
            // sessions, the MCP disconnect path also does this (crash-
            // safety fallback) — whichever runs first wins, the
            // second is a no-op.
            if !is_session_correlated(&correlation, &session_id) {
                ctx.sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&session_id);
            }

            // Sync the reduced root set.
            let global = tracker.global_roots();
            if let Err(e) = ctx.primary.sync_roots(global).await {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    "root sync after session end failed: {e}",
                );
            }

            info!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                "session ended: roots cleaned up",
            );
        }

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            None,
            &raw.to_string(),
            "incoming hook",
        );
        let parent_str = id.0.to_string();
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            Some(&parent_str),
            "",
            "outgoing hook response",
        );

        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Start editing confirmation ────────────────────────────────
    //
    // `start-editing/confirm` is sent by `catenary start_editing`
    // after the PreToolUse hook has already entered editing mode.
    // The CLI command just needs a confirmation response.
    if method == "tool/start-editing" {
        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Done editing handoff: prepare ────────────────────────────
    //
    // `pre-tool/done-editing-prepare` is sent by the PreToolUse
    // hook when the agent runs `catenary done_editing`. Acquires
    // the handoff lock, drains files, releases the editing guardrail,
    // and deposits the file list for the subsequent CLI command.
    if method == "pre-tool/done-editing" {
        let id = ctx.logging.next_id();

        let router = get_or_create_router(&ctx, &session_id);

        // Acquire the handoff semaphore (blocks if another session
        // is mid-handoff — holds for milliseconds at most).
        let permit = ctx
            .handoff_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("handoff semaphore closed"))?;

        // Drain accumulated files from EditingManager.
        let files = router.session.editing.drain_all_and_clear();

        // Release the editing guardrail.
        ctx.editing_guardrail.release_all(&session_id);

        // Deposit in the handoff slot.
        {
            let mut slot = ctx
                .handoff_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(HandoffContext { files, permit });
        }

        // Spawn timeout to clear the slot if the CLI never connects.
        spawn_handoff_timeout(ctx.handoff_slot.clone());

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            "done_editing handoff prepared",
        );

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            None,
            &raw.to_string(),
            "incoming hook",
        );
        let parent_str = id.0.to_string();
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            Some(&parent_str),
            "{\"status\":\"ok\"}",
            "outgoing hook response",
        );

        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Done editing handoff: run ────────────────────────────────
    //
    // `done-editing/run` is sent by `catenary done_editing` CLI
    // command. Takes the file list from the handoff slot, runs
    // process_files_batched, and returns diagnostics.
    if method == "tool/done-editing" {
        let id = ctx.logging.next_id();

        // Take the file list from the handoff slot and release the
        // permit immediately. The permit must not be held during the
        // diagnostics pipeline (which may take seconds).
        let files = {
            let mut slot = ctx
                .handoff_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Destructure HandoffContext — dropping it releases the
            // owned semaphore permit.
            slot.take().map(|h| h.files)
        };

        let response = if let Some(files) = files {
            if files.is_empty() {
                "[no files modified]\n".to_string()
            } else {
                ctx.primary
                    .diagnostics
                    .process_files_batched(&files, None)
                    .await
            }
        } else {
            // Handoff slot was empty — timeout expired or double-consume.
            "done_editing handoff expired — no files available\n".to_string()
        };

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            id.0,
            None,
            &raw.to_string(),
            "incoming hook",
        );
        let parent_str = id.0.to_string();
        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            id.0,
            Some(&parent_str),
            &response,
            "outgoing hook response",
        );

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Root management ──────────────────────────────────────────
    //
    // `add-root/run` and `rm-root/run` are sent by the CLI commands
    // (`catenary add-root`, `catenary rm-root`). The PreToolUse hook
    // only bypasses the command filter — no hook-side IPC needed
    // since "hook" is a shared contributor with no session identity.
    //
    // Handled before `get_or_create_router` because root management
    // is a daemon-level concern (RootTracker), not a per-session
    // router concern.
    if method == "tool/add-root" {
        let id = ctx.logging.next_id();
        let response = if let Some(path_str) = raw.get("path").and_then(|v| v.as_str()) {
            let path = PathBuf::from(path_str);
            let canonical = path.canonicalize().unwrap_or(path);
            if let Some(ref tracker) = ctx.root_tracker {
                tracker.add_roots("hook", std::slice::from_ref(&canonical));
                let global = tracker.global_roots();
                if let Err(e) = ctx.primary.sync_roots(global).await {
                    debug!(
                        source = Source::DaemonDispatch.as_str(),
                        "root sync after add-root failed: {e}",
                    );
                }
                info!(
                    source = Source::DaemonDispatch.as_str(),
                    path = %canonical.display(),
                    "added root via hook contributor",
                );
            }
            serde_json::json!({"status": "ok", "path": canonical.display().to_string()})
        } else {
            serde_json::json!({"status": "error", "message": "missing path"})
        };

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            None,
            &raw.to_string(),
            "incoming hook",
        );
        let parent_str = id.0.to_string();
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            Some(&parent_str),
            &response.to_string(),
            "outgoing hook response",
        );

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    if method == "tool/rm-root" {
        let id = ctx.logging.next_id();
        let response = if let Some(path_str) = raw.get("path").and_then(|v| v.as_str()) {
            let path = PathBuf::from(path_str);
            let canonical = path.canonicalize().unwrap_or(path);
            if let Some(ref tracker) = ctx.root_tracker {
                let removed = tracker.remove_root("hook", &canonical);
                if removed {
                    let global = tracker.global_roots();
                    if let Err(e) = ctx.primary.sync_roots(global).await {
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            "root sync after rm-root failed: {e}",
                        );
                    }
                    info!(
                        source = Source::DaemonDispatch.as_str(),
                        path = %canonical.display(),
                        "removed root from hook contributor",
                    );
                    serde_json::json!({"status": "ok", "path": canonical.display().to_string()})
                } else {
                    serde_json::json!({
                        "status": "not_found",
                        "message": format!(
                            "root not found in hook-managed roots: {}",
                            canonical.display()
                        )
                    })
                }
            } else {
                serde_json::json!({"status": "error", "message": "no root tracker"})
            }
        } else {
            serde_json::json!({"status": "error", "message": "missing path"})
        };

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            None,
            &raw.to_string(),
            "incoming hook",
        );
        let parent_str = id.0.to_string();
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            id.0,
            Some(&parent_str),
            &response.to_string(),
            "outgoing hook response",
        );

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    let router = get_or_create_router(&ctx, &session_id);

    // Span with session_id so warn!/error! events emitted during
    // hook dispatch route to the correct notification queue.
    let hook_span = tracing::info_span!(
        "hook_dispatch",
        session_id = %session_id,
    );
    let _hook_guard = hook_span.enter();

    // Mint a correlation ID for this request/response pair.
    let id = ctx.logging.next_id();

    let request: HookRequest =
        serde_json::from_value(raw.clone()).map_err(|e| anyhow!("Invalid hook request: {e}"))?;

    let result = router.dispatch(request, id.0);

    // Bridge roots refresh: if PreAgent set the per-session flag and
    // this session has a correlated MCP connection, propagate the
    // signal to the connection's flag so the MCP run loop sees it.
    if method == "pre-agent/turn-start"
        && let Some(flag) = correlation
            .roots_refresh_flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
    {
        flag.store(true, Ordering::Release);
    }

    let envelope = HookResponseEnvelope {
        result: result.result,
        system_message: result.system_message,
    };
    let response = if envelope.result.is_some() || envelope.system_message.is_some() {
        serde_json::to_string(&envelope)?
    } else {
        String::new()
    };

    // Determine level from outcome and hook category.
    let level = hook_outcome_level(&method, &envelope);

    // Log incoming hook request (deferred — uses outcome-determined level).
    emit_hook_event(
        level,
        &session_id,
        &method,
        id.0,
        None,
        &raw.to_string(),
        "incoming hook",
    );

    // Log outgoing hook response.
    let response_parent_str = id.0.to_string();
    emit_hook_event(
        level,
        &session_id,
        &method,
        id.0,
        Some(&response_parent_str),
        &response,
        "outgoing hook response",
    );

    writer.write_all(response.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;

    Ok(())
}

#[cfg(unix)]
impl Drop for SessionManager {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.mcp_socket_path);
        let _ = std::fs::remove_file(&self.hook_socket_path);
    }
}

// ── Bridge proxy ────────────────────────────────────────────────────

/// Maximum number of attempts to connect to the daemon.
const MAX_CONNECT_ATTEMPTS: u32 = 10;

/// Delay between connection retry attempts.
const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Runs the bridge proxy: connect-or-start the daemon, then proxy
/// stdin/stdout to/from the daemon socket.
///
/// Entirely synchronous — no tokio runtime involvement in the data
/// path. This avoids any interaction between the tokio runtime's
/// internal epoll/signal state and the blocking I/O threads.
///
/// # Errors
///
/// Returns an error if the daemon cannot be started, the connection
/// fails, or the daemon closes the connection before stdin.
#[cfg(unix)]
pub fn run_bridge() -> Result<()> {
    // Guard against recursive spawning. If the daemon subprocess
    // somehow enters the bridge path (e.g., the "daemon" arg is lost),
    // this prevents an infinite process chain.
    if std::env::var_os("_CATENARY_BRIDGE").is_some() {
        anyhow::bail!(
            "recursive bridge detected — the daemon subprocess \
             re-entered the bridge path instead of the daemon path"
        );
    }
    let stream = connect_or_start_daemon()?;
    proxy_stdio(stream)
}

/// Connects to a running daemon or starts one.
///
/// Implements the start-or-connect sequence:
/// 1. Try to connect to the MCP socket.
/// 2. If connection fails and a stale socket file exists, remove it.
/// 3. Spawn a daemon process (`catenary daemon`).
/// 4. Retry connection with backoff.
///
/// # Errors
///
/// Returns an error if the daemon cannot be reached after all retry attempts.
#[cfg(unix)]
fn connect_or_start_daemon() -> Result<std::os::unix::net::UnixStream> {
    let socket_path = mcp_socket_path();
    let mut daemon_spawned = false;

    for attempt in 0..MAX_CONNECT_ATTEMPTS {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(&socket_path) {
            info!(
                source = Source::DaemonLifecycle.as_str(),
                attempt, "connected to daemon",
            );
            return Ok(stream);
        }

        let last_attempt = attempt == MAX_CONNECT_ATTEMPTS - 1;
        if last_attempt {
            anyhow::bail!(
                "failed to connect to Catenary daemon \
                 after {MAX_CONNECT_ATTEMPTS} attempts ({})",
                socket_path.display(),
            );
        }

        if !daemon_spawned {
            if socket_path.exists() {
                let _ = std::fs::remove_file(&socket_path);
            }
            let hook_path = hook_socket_path();
            if hook_path.exists() {
                let _ = std::fs::remove_file(&hook_path);
            }
            spawn_daemon()?;
            daemon_spawned = true;
        }

        std::thread::sleep(CONNECT_RETRY_DELAY);
    }

    anyhow::bail!(
        "failed to connect to Catenary daemon ({})",
        socket_path.display(),
    )
}

/// Spawns `catenary daemon` as a detached child process.
///
/// The daemon binds the MCP socket and begins accepting connections.
/// Uses a new process group so the daemon outlives the bridge. Stderr
/// is redirected to `$XDG_STATE_HOME/catenary/daemon.log` so that
/// daemon crashes during initialization are diagnosable from the
/// bridge side (and from integration test failure output).
#[cfg(unix)]
fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("resolve current executable path")?;

    let log_dir = crate::db::state_dir().join("catenary");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("create daemon log directory: {}", log_dir.display()))?;
    let log_path = log_dir.join("daemon.log");
    let stderr_file = std::fs::File::create(&log_path)
        .with_context(|| format!("create daemon log: {}", log_path.display()))?;

    Command::new(exe)
        .arg("daemon")
        .env("_CATENARY_BRIDGE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .process_group(0)
        .spawn()
        .context("spawn daemon process")?;

    Ok(())
}

/// Proxies stdin/stdout to/from a daemon socket connection.
///
/// Before entering the concurrent proxy, intercepts the first MCP
/// exchange (initialize) to verify the daemon's version matches this
/// bridge's version. On mismatch, returns an error without proxying.
///
/// Uses purely blocking I/O on two threads: one copies stdin to the
/// daemon socket, the other copies the daemon socket to stdout. Both
/// threads share the same socket fd via `try_clone()` — this is safe
/// because both halves stay in blocking mode (no mixed
/// blocking/non-blocking on the same file description). Full-duplex
/// Unix sockets support concurrent read and write from different
/// threads.
///
/// The socket→stdout direction uses a read-write-flush loop because
/// `std::io::Stdout` uses full buffering on pipes. Without explicit
/// flushing, MCP responses sit in the buffer until it fills (8 KB).
///
/// Returns `Ok(())` when stdin closes (host CLI ended the session).
/// Returns `Err` when the daemon connection drops first (unexpected).
///
/// # Errors
///
/// Returns an error if the daemon version does not match, or if the
/// daemon connection closes before stdin (unexpected termination).
#[cfg(unix)]
fn proxy_stdio(stream: std::os::unix::net::UnixStream) -> Result<()> {
    use std::io::{Read, Write};

    // Phase 1: Version handshake (blocking, sequential).
    // Intercepts the first MCP exchange (initialize) to verify that the
    // daemon version matches this bridge. On mismatch, the handshake
    // sends a catenary/version-mismatch notification to the daemon and
    // returns Err.
    {
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        version_handshake(&mut stdin, &stream, &mut stdout)?;
    }

    // Phase 2: Concurrent byte proxy for remaining messages.
    let writer = stream.try_clone().context("clone daemon socket")?;
    let reader = stream;

    // stdin → socket: dedicated thread, blocks until stdin EOF.
    let stdin_thread = std::thread::spawn(move || -> Result<()> {
        let mut stdin = std::io::stdin().lock();
        let mut w = writer;
        std::io::copy(&mut stdin, &mut w).context("proxy stdin to socket")?;
        let _ = w.shutdown(std::net::Shutdown::Write);
        Ok(())
    });

    // socket → stdout: runs on calling thread, blocks until socket EOF.
    let mut stdout = std::io::stdout().lock();
    let mut buf = vec![0u8; 8192];
    let mut r = reader;
    let stdout_result: Result<()> = loop {
        match r.read(&mut buf) {
            Ok(0) => break Err(anyhow::anyhow!("daemon connection closed unexpectedly")),
            Ok(n) => {
                if let Err(e) = stdout.write_all(&buf[..n]) {
                    break Err(anyhow::Error::from(e).context("write to stdout"));
                }
                if let Err(e) = stdout.flush() {
                    break Err(anyhow::Error::from(e).context("flush stdout"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                // stdout pipe closed (host killed the process).
                break Ok(());
            }
            Err(e) => break Err(anyhow::Error::from(e).context("read from daemon")),
        }
    };

    // If we got here, the socket→stdout loop ended. Either the daemon
    // died (Err) or stdout pipe broke (Ok). The stdin thread may still
    // be blocked; don't join it — the process is exiting.
    //
    // If stdin closed first, the stdin thread already exited and the
    // daemon will close the connection → we exit via the read loop above.
    drop(stdin_thread);

    stdout_result
}

/// Intercepts the MCP initialize handshake to verify daemon version.
///
/// Reads the initialize request from `client`, forwards it to `socket`,
/// reads the response, and checks `serverInfo.version` against this
/// bridge's version ([`CATENARY_VERSION`](env!("CATENARY_VERSION"))).
/// On match, forwards the response to `output`. On mismatch, sends a
/// `catenary/version-mismatch` notification to the daemon and returns
/// an error.
///
/// Generic over reader/writer for testability — `proxy_stdio` passes
/// stdin/stdout, tests pass in-memory buffers.
#[cfg(unix)]
fn version_handshake<R: std::io::BufRead, W: std::io::Write>(
    client: &mut R,
    socket: &std::os::unix::net::UnixStream,
    output: &mut W,
) -> Result<()> {
    use std::io::Write;

    // Read the initialize request from the client (one JSON-RPC line).
    let mut init_line = String::new();
    client
        .read_line(&mut init_line)
        .context("read initialize request from client")?;

    // Forward to daemon.
    (&*socket)
        .write_all(init_line.as_bytes())
        .context("forward initialize request to daemon")?;
    (&*socket).flush()?;

    // Read the initialize response from daemon.
    // Byte-by-byte to avoid consuming data beyond the line boundary,
    // which would be lost to the subsequent concurrent byte proxy.
    let response_line = read_json_line(socket).context("read initialize response from daemon")?;

    // Parse and check version.
    let response: serde_json::Value =
        serde_json::from_str(response_line.trim()).context("parse initialize response")?;

    let daemon_version = response
        .pointer("/result/serverInfo/version")
        .and_then(|v| v.as_str());

    let bridge_version = env!("CATENARY_VERSION");

    match daemon_version {
        Some(dv) if dv == bridge_version => {}
        Some(dv) => {
            // Notify daemon of the mismatch before disconnecting so it
            // can surface the event via the notification sink.
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "catenary/version-mismatch",
                "params": { "bridgeVersion": bridge_version }
            });
            if let Ok(line) = serde_json::to_string(&notification) {
                let _ = (&*socket).write_all(line.as_bytes());
                let _ = (&*socket).write_all(b"\n");
                let _ = (&*socket).flush();
            }

            anyhow::bail!(
                "Catenary version mismatch: daemon is v{dv}, \
                 bridge is v{bridge_version}. Run 'catenary stop' and retry."
            );
        }
        None => {
            anyhow::bail!(
                "daemon did not report a version in serverInfo — \
                 not a Catenary daemon or a bug"
            );
        }
    }

    // Version matches — forward the response to the client.
    output
        .write_all(response_line.as_bytes())
        .context("forward initialize response to client")?;
    output.flush()?;

    Ok(())
}

/// Reads a single newline-terminated line from a socket without buffering.
///
/// Reads byte-by-byte so that no data beyond the line boundary is consumed
/// from the kernel's receive buffer, which is shared across all handles to
/// the same file descriptor.
#[cfg(unix)]
fn read_json_line(socket: &std::os::unix::net::UnixStream) -> Result<String> {
    use std::io::Read;

    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    loop {
        (&*socket)
            .read_exact(&mut byte)
            .context("read byte from socket")?;
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(buf).context("response is not valid UTF-8")
}

#[cfg(test)]
#[cfg(unix)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "tests intentionally hold SessionManager alive for socket lifetime"
)]
mod tests {
    use super::*;
    use crate::logging::LoggingServer;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    /// Minimal no-op handler for tests that only exercise transport.
    #[derive(Clone)]
    struct NoOpHandler;
    impl crate::mcp::ToolHandler for NoOpHandler {
        fn list_tools(&self) -> Vec<crate::mcp::Tool> {
            Vec::new()
        }
        fn call_tool(
            &self,
            _name: &str,
            _arguments: Option<serde_json::Value>,
            _parent_id: Option<String>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<crate::mcp::CallToolResult> {
            Err(anyhow::anyhow!("not implemented"))
        }
    }

    /// Create an MCP socket path inside a tempdir.
    fn mcp_socket_in(dir: &Path) -> PathBuf {
        dir.join("catenary").join("catenary.sock")
    }

    /// Create a hook socket path inside a tempdir.
    fn hook_socket_in(dir: &Path) -> PathBuf {
        dir.join("catenary").join("catenary-hooks.sock")
    }

    /// Bind a `SessionManager` with both sockets in a tempdir.
    fn bind_in(dir: &Path) -> SessionManager {
        SessionManager::bind_at(
            &mcp_socket_in(dir),
            &hook_socket_in(dir),
            Arc::new(NoOpHandler),
            LoggingServer::new(),
        )
        .expect("bind")
    }

    // ── Tracing capture layer ──────────────────────────────────────

    /// Minimal tracing layer that captures `source` field values.
    struct CaptureLayer {
        sources: Arc<Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(Option<String>);

            impl tracing::field::Visit for Visitor {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "source" {
                        self.0 = Some(value.to_string());
                    }
                }

                fn record_debug(
                    &mut self,
                    _field: &tracing::field::Field,
                    _value: &dyn std::fmt::Debug,
                ) {
                }
            }

            let mut v = Visitor(None);
            event.record(&mut v);
            if let Some(src) = v.0
                && let Ok(mut sources) = self.sources.lock()
            {
                sources.push(src);
            }
        }
    }

    // ── Tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn bind_creates_socket_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let _manager = bind_in(dir.path());

        assert!(mcp_path.exists(), "MCP socket file should exist after bind");
    }

    #[tokio::test]
    async fn accept_connection() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(manager.connection_count(), 1);

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn multiple_connections() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let streams: Vec<_> = {
            let mut v = Vec::new();
            for _ in 0..3 {
                v.push(
                    tokio::net::UnixStream::connect(&mcp_path)
                        .await
                        .expect("connect"),
                );
            }
            v
        };

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(manager.connection_count(), 3);

        drop(streams);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn drop_removes_socket() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        let manager = bind_in(dir.path());
        assert!(mcp_path.exists(), "MCP socket should exist before drop");
        assert!(hook_path.exists(), "hook socket should exist before drop");

        drop(manager);

        assert!(
            !mcp_path.exists(),
            "MCP socket should be removed after drop"
        );
        assert!(
            !hook_path.exists(),
            "hook socket should be removed after drop"
        );
    }

    #[tokio::test]
    async fn bind_fails_if_mcp_socket_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        // Create a regular file at the MCP socket path.
        std::fs::create_dir_all(mcp_path.parent().expect("parent")).expect("create dir");
        std::fs::write(&mcp_path, b"").expect("create file");

        let result = SessionManager::bind_at(
            &mcp_path,
            &hook_path,
            Arc::new(NoOpHandler),
            LoggingServer::new(),
        );
        assert!(
            result.is_err(),
            "bind should fail when MCP socket already exists"
        );
    }

    #[tokio::test]
    async fn bind_fails_if_hook_socket_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        // Create a regular file at the hook socket path.
        std::fs::create_dir_all(hook_path.parent().expect("parent")).expect("create dir");
        std::fs::write(&hook_path, b"").expect("create file");

        let result = SessionManager::bind_at(
            &mcp_path,
            &hook_path,
            Arc::new(NoOpHandler),
            LoggingServer::new(),
        );
        assert!(
            result.is_err(),
            "bind should fail when hook socket already exists"
        );
    }

    #[tokio::test]
    async fn startup_tracing_event() {
        let sources = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            sources: Arc::clone(&sources),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        let _manager = tracing::subscriber::with_default(subscriber, || {
            SessionManager::bind_at(
                &mcp_path,
                &hook_path,
                Arc::new(NoOpHandler),
                LoggingServer::new(),
            )
        })
        .expect("bind");

        let captured = sources.lock().expect("lock").clone();
        assert!(
            captured.contains(&"daemon.lifecycle".to_string()),
            "should emit daemon.lifecycle event, got: {captured:?}",
        );
    }

    // ── Bridge proxy tests ────────────────────────────────────────────

    #[tokio::test]
    async fn bridge_cleans_stale_socket() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        // Create a stale socket file (regular file, nobody listening).
        std::fs::create_dir_all(mcp_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&mcp_path, b"stale").expect("create stale file");

        // Connect should fail on a regular file.
        let result = tokio::net::UnixStream::connect(&mcp_path).await;
        assert!(result.is_err());

        // Clean stale file (what connect_or_start_daemon does).
        std::fs::remove_file(&mcp_path).expect("remove stale");
        assert!(!mcp_path.exists());

        // Now bind succeeds.
        let _manager = bind_in(dir.path());
        assert!(mcp_path.exists());
    }

    #[tokio::test]
    async fn bridge_proxies_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("proxy.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");

        let client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connect");
        let (mut server, _) = listener.accept().await.expect("accept");

        let (mut client_read, mut client_write) = client.into_split();

        // Client → server direction.
        client_write.write_all(b"hello").await.expect("write");
        client_write.shutdown().await.expect("shutdown write");

        let mut buf = vec![0u8; 5];
        server.read_exact(&mut buf).await.expect("server read");
        assert_eq!(&buf, b"hello");

        // Server → client direction.
        server.write_all(b"world").await.expect("server write");
        server.shutdown().await.expect("shutdown server");

        let mut response = Vec::new();
        client_read
            .read_to_end(&mut response)
            .await
            .expect("client read");
        assert_eq!(&response, b"world");
    }

    #[tokio::test]
    async fn bridge_exits_on_daemon_death() {
        use tokio::io::AsyncReadExt;

        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("death.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");

        let client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connect");
        let (server, _) = listener.accept().await.expect("accept");

        // Simulate daemon death.
        drop(server);
        drop(listener);

        let mut buf = Vec::new();
        let mut client = client;
        let n = client
            .read_to_end(&mut buf)
            .await
            .expect("read after daemon death");
        assert_eq!(n, 0, "bridge should see EOF when daemon dies");
    }

    #[tokio::test]
    async fn bridge_handles_race() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Spawn 5 connections concurrently. Hold them alive so the
        // per-connection MCP tasks don't exit (EOF → count decrement).
        let mut handles = Vec::new();
        for _ in 0..5 {
            let p = mcp_path.clone();
            handles.push(tokio::spawn(async move {
                tokio::net::UnixStream::connect(&p).await
            }));
        }

        let mut streams = Vec::new();
        for handle in handles {
            streams.push(handle.await.expect("task").expect("connect"));
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 5);

        drop(streams);
        shutdown.cancel();
    }

    // ── Hook socket tests ────────────────────────────────────────────

    #[tokio::test]
    async fn hook_socket_created_on_bind() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let _manager = bind_in(dir.path());

        assert!(
            hook_path.exists(),
            "hook socket file should exist after bind"
        );
    }

    #[tokio::test]
    async fn hook_connection_accepted() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _stream = tokio::net::UnixStream::connect(&hook_path)
            .await
            .expect("connect to hook socket");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn hook_and_mcp_sockets_independent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Connect to both sockets simultaneously.
        let (mcp_result, hook_result) = tokio::join!(
            tokio::net::UnixStream::connect(&mcp_path),
            tokio::net::UnixStream::connect(&hook_path),
        );

        let mcp_stream = mcp_result.expect("connect to MCP socket");
        let _hook_stream = hook_result.expect("connect to hook socket");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Only MCP connections are tracked.
        assert_eq!(manager.connection_count(), 1);

        drop(mcp_stream);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn hook_passthrough_response() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = tokio::net::UnixStream::connect(&hook_path)
            .await
            .expect("connect");
        let (reader, mut writer) = stream.into_split();

        // Send a hook request.
        let request = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "agent_id": "",
            "session_id": "test-session"
        });
        let mut payload = serde_json::to_string(&request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        // Read the passthrough response (empty line = allow).
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");
        assert_eq!(line.trim(), "", "passthrough should return empty response");

        shutdown.cancel();
    }

    // ── Per-connection MCP stack tests ────────────────────────────────

    /// Handler that echoes its tool list and tool calls (for MCP testing).
    #[derive(Clone)]
    struct EchoHandler;
    impl crate::mcp::ToolHandler for EchoHandler {
        fn list_tools(&self) -> Vec<crate::mcp::Tool> {
            vec![crate::mcp::Tool {
                name: "echo".to_string(),
                title: Some("Echo Tool".to_string()),
                description: Some("Returns a fixed string".to_string()),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                annotations: None,
            }]
        }
        fn call_tool(
            &self,
            _name: &str,
            _arguments: Option<serde_json::Value>,
            _parent_id: Option<String>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<crate::mcp::CallToolResult> {
            Ok(crate::mcp::CallToolResult::text("echo"))
        }
    }

    /// Bind a `SessionManager` with an `EchoHandler` for MCP-level tests.
    fn bind_echo(dir: &Path) -> SessionManager {
        SessionManager::bind_at(
            &mcp_socket_in(dir),
            &hook_socket_in(dir),
            Arc::new(EchoHandler),
            LoggingServer::new(),
        )
        .expect("bind")
    }

    /// Helper: send JSON line, read JSON line response over a std stream.
    fn mcp_roundtrip(
        stream: &std::os::unix::net::UnixStream,
        request: &serde_json::Value,
    ) -> serde_json::Value {
        use std::io::{BufRead, Write};
        let mut buf_writer = std::io::BufWriter::new(stream.try_clone().expect("clone"));
        let line = serde_json::to_string(request).expect("serialize");
        writeln!(buf_writer, "{line}").expect("write");
        buf_writer.flush().expect("flush");

        let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
        let mut response_line = String::new();
        reader.read_line(&mut response_line).expect("read");
        serde_json::from_str(response_line.trim()).expect("parse response")
    }

    #[tokio::test]
    async fn transport_agnostic_mcp() {
        use std::io::{BufRead, Write};

        // Run McpServer with a Unix stream pair (in-process, no filesystem).
        let (server_stream, client_stream) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");
        let reader = server_stream.try_clone().expect("clone for reader");
        let writer = server_stream;

        let logging = LoggingServer::new();
        let handle = std::thread::spawn(move || {
            let mut mcp = McpServer::new(EchoHandler, logging);
            mcp.run(reader, writer)
        });

        // Client side: send initialize.
        let mut client_writer = std::io::BufWriter::new(client_stream.try_clone().expect("clone"));
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        });
        let line = serde_json::to_string(&init).expect("serialize");
        writeln!(client_writer, "{line}").expect("write");
        client_writer.flush().expect("flush");

        // Read response.
        let mut client_reader = std::io::BufReader::new(&client_stream);
        let mut response_line = String::new();
        client_reader.read_line(&mut response_line).expect("read");
        let response: serde_json::Value =
            serde_json::from_str(response_line.trim()).expect("parse");

        assert!(response.get("result").is_some(), "should have result");
        assert_eq!(response["result"]["serverInfo"]["name"], "catenary");

        // Close client to signal EOF → server exits.
        drop(client_writer);
        drop(client_stream);
        handle.join().expect("server thread").expect("server run");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_connection_mcp_initialize() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Connect and send MCP initialize.
        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set timeout");

        let response = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            }),
        );

        assert!(
            response.get("result").is_some(),
            "expected result in initialize response, got: {response}",
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "catenary");

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_connection_tools_list() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set timeout");

        // Initialize first.
        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            }),
        );

        // List tools.
        let response = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }),
        );

        let tools = response["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo");

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_cleanup_on_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 1, "should have 1 connection");

        // Disconnect.
        drop(stream);

        // Wait for cleanup (MCP server detects EOF and task exits).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            manager.connection_count(),
            0,
            "connection should be cleaned up after disconnect"
        );

        shutdown.cancel();
    }

    // ── Lifecycle tests ─────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_on_last_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Connect one client.
        let stream = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 1);

        // Disconnect — last client gone, accept_loop should exit.
        drop(stream);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");

        // Sockets removed so new bridges start a fresh daemon.
        assert!(
            !mcp_path.exists(),
            "MCP socket should be removed after shutdown",
        );
        assert!(
            !hook_path.exists(),
            "hook socket should be removed after shutdown",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_with_multiple_clients() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Connect two clients.
        let stream1 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 1");
        let stream2 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 2");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 2);

        // Disconnect first — daemon should stay alive.
        drop(stream1);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "accept_loop should still be running with one client",
        );

        // Disconnect second — daemon should exit.
        drop(stream2);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");
    }

    #[tokio::test]
    async fn stop_via_hook_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Send shutdown via hook socket.
        let stream = tokio::net::UnixStream::connect(&hook_path)
            .await
            .expect("connect to hook socket");
        let (reader, mut writer) = stream.into_split();

        let request = serde_json::json!({"method": "tool/shutdown"});
        let mut payload = serde_json::to_string(&request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        // Read the ack response.
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");
        assert!(
            line.contains("ok"),
            "should receive ok response, got: {line}",
        );

        // accept_loop should exit.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");

        // Sockets removed so new bridges start a fresh daemon.
        assert!(
            !mcp_path.exists(),
            "MCP socket should be removed after stop",
        );
        assert!(
            !hook_path.exists(),
            "hook socket should be removed after stop",
        );
    }

    #[tokio::test]
    async fn shutdown_token_exits_accept_loop() {
        let dir = tempfile::tempdir().expect("create tempdir");

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Cancel the token directly (simulates signal handling).
        shutdown.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");
    }

    // ── Session state tests ─────────────────────────────────────────────

    /// Create a `SessionManager` with a real `Session` for hook dispatch tests.
    fn bind_with_session(dir: &Path) -> SessionManager {
        let db_path = dir.join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&db_path).expect("open test DB");
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('daemon', 1, 'test-daemon', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let logging = LoggingServer::new();
        let runtime = tokio::runtime::Handle::current();
        let instance_id: Arc<str> = "daemon".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default(),
            vec![],
            logging.clone(),
            conn.clone(),
            instance_id,
            runtime,
            notification_router,
        ));

        SessionManager::bind_at(
            &mcp_socket_in(dir),
            &hook_socket_in(dir),
            Arc::new(NoOpHandler),
            logging,
        )
        .expect("bind")
        .with_session(session, conn)
    }

    /// Send a hook JSON request and read the response line.
    async fn hook_roundtrip(hook_path: &Path, request: &serde_json::Value) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stream = tokio::net::UnixStream::connect(hook_path)
            .await
            .expect("connect to hook socket");
        let (reader, mut writer) = stream.into_split();

        let mut payload = serde_json::to_string(request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");
        line
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_hook_creates_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        assert_eq!(manager.session_count(), 0, "no sessions initially");

        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send a hook with session_id = "abc".
        let request = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "abc"
        });
        let _ = hook_roundtrip(&hook_path, &request).await;

        assert_eq!(
            manager.session_count(),
            1,
            "session 'abc' should exist in registry"
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_hook_routes_to_correct_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send hooks with two different session_ids.
        let req_a = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "session-a"
        });
        let req_b = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "session-b"
        });
        let _ = hook_roundtrip(&hook_path, &req_a).await;
        let _ = hook_roundtrip(&hook_path, &req_b).await;

        assert_eq!(
            manager.session_count(),
            2,
            "should have two independent sessions"
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_editing_per_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Session A: enter editing mode via CLI start_editing hook.
        let req = serde_json::json!({
            "method": "pre-tool/start-editing",
            "agent_id": "",
            "session_id": "session-a"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        // Session A: Edit should be allowed (in editing mode).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "agent_id": "",
            "session_id": "session-a"
        });
        let line = hook_roundtrip(&hook_path, &req).await;
        assert_eq!(
            line.trim(),
            "",
            "session A should allow Edit (in editing mode)"
        );

        // Session B: Edit should be denied (not in editing mode).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "agent_id": "",
            "session_id": "session-b"
        });
        let line = hook_roundtrip(&hook_path, &req).await;
        let envelope: crate::hook::HookResponseEnvelope =
            serde_json::from_str(line.trim()).expect("parse response");
        assert!(
            matches!(envelope.result, Some(crate::hook::HookResult::Deny(_))),
            "session B should deny Edit (not editing), got: {envelope:?}"
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_turn_counter_per_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send two turn-start hooks to session A.
        let req_a = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "session-a"
        });
        let _ = hook_roundtrip(&hook_path, &req_a).await;
        let _ = hook_roundtrip(&hook_path, &req_a).await;

        // Send one turn-start hook to session B.
        let req_b = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "session-b"
        });
        let _ = hook_roundtrip(&hook_path, &req_b).await;

        // Verify each session has its own turn counter by checking
        // that session A and B exist independently.
        assert_eq!(manager.session_count(), 2);

        // Verify independence through the hook_ctx.
        let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
        let sessions = ctx.sessions.lock().expect("lock");
        let router_a = Arc::clone(&sessions.get("session-a").expect("session-a").router);
        let router_b = Arc::clone(&sessions.get("session-b").expect("session-b").router);
        drop(sessions);
        assert_eq!(router_a.turn(), 2, "session A should have turn 2");
        assert_eq!(router_b.turn(), 1, "session B should have turn 1");

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_subagent_passthrough() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Hook with non-empty agent_id should pass through without
        // triggering diagnostics or editing enforcement.
        let req = serde_json::json!({
            "method": "post-tool/diagnostics",
            "file": "/tmp/test.rs",
            "agent_id": "sub-agent-1",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&hook_path, &req).await;
        assert_eq!(
            line.trim(),
            "",
            "subagent hook should pass through (empty response)"
        );

        shutdown.cancel();
    }

    // ── Done editing handoff tests ────────────────────────────────────

    /// Send a hook JSON request and read all response data (may be
    /// multi-line, unlike `hook_roundtrip` which reads a single line).
    async fn hook_roundtrip_full(hook_path: &Path, request: &serde_json::Value) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::UnixStream::connect(hook_path)
            .await
            .expect("connect to hook socket");

        let mut payload = serde_json::to_string(request).expect("serialize");
        payload.push('\n');
        stream.write_all(payload.as_bytes()).await.expect("write");
        stream.shutdown().await.expect("shutdown write");

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read response");
        response
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_no_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/start-editing",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        // Prepare handoff (no files accumulated).
        let req = serde_json::json!({
            "method": "pre-tool/done-editing",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&hook_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — should get "no files modified".
        let req = serde_json::json!({"method": "tool/done-editing"});
        let response = hook_roundtrip_full(&hook_path, &req).await;
        assert!(
            response.contains("no files modified"),
            "expected 'no files modified', got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_expired() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Call done-editing/run without preparing a handoff.
        let req = serde_json::json!({"method": "tool/done-editing"});
        let response = hook_roundtrip_full(&hook_path, &req).await;
        assert!(
            response.contains("handoff expired"),
            "expected handoff expired message, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_with_accumulated_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/start-editing",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        // Accumulate a file via post-tool hook.
        let req = serde_json::json!({
            "method": "post-tool/diagnostics",
            "file": "/tmp/nonexistent_file.rs",
            "tool": "Edit",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        // Prepare handoff — should drain the accumulated file.
        let req = serde_json::json!({
            "method": "pre-tool/done-editing",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&hook_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — diagnostics pipeline runs on
        // the files. Since there's no real LSP server, the output
        // depends on whether the file exists and has LSP coverage.
        // The key test: the handoff consumed the files successfully.
        let req = serde_json::json!({"method": "tool/done-editing"});
        let response = hook_roundtrip_full(&hook_path, &req).await;
        // With no LSP servers, process_files_batched returns "[clean]"
        // for files without coverage. The response should not be the
        // expired message.
        assert!(
            !response.contains("handoff expired"),
            "handoff should not be expired, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_double_consume() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode and prepare handoff.
        let req = serde_json::json!({
            "method": "pre-tool/start-editing",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        let req = serde_json::json!({
            "method": "pre-tool/done-editing",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        // First consume should succeed.
        let req = serde_json::json!({"method": "tool/done-editing"});
        let response1 = hook_roundtrip_full(&hook_path, &req).await;
        assert!(
            !response1.contains("handoff expired"),
            "first consume should succeed, got: {response1}",
        );

        // Second consume should see expired slot.
        let response2 = hook_roundtrip_full(&hook_path, &req).await;
        assert!(
            response2.contains("handoff expired"),
            "second consume should see expired slot, got: {response2}",
        );

        shutdown.cancel();
    }

    // ── Version handshake tests ──────────────────────────────────────

    /// Helper: build an initialize request JSON line.
    fn init_request_line() -> String {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        });
        format!("{}\n", serde_json::to_string(&request).expect("serialize"))
    }

    /// Spawn a fake daemon thread that reads an initialize request and
    /// responds with the given version in `serverInfo`.
    fn fake_daemon(
        stream: std::os::unix::net::UnixStream,
        version: &str,
    ) -> std::thread::JoinHandle<()> {
        let version = version.to_string();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut reader = std::io::BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read init request");

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {
                        "name": "catenary",
                        "version": version,
                    }
                }
            });
            let mut w: &std::os::unix::net::UnixStream = &stream;
            writeln!(w, "{}", serde_json::to_string(&response).expect("ser"))
                .expect("write response");
        })
    }

    #[test]
    fn matching_version_connects() {
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        let handle = fake_daemon(server_sock, env!("CATENARY_VERSION"));

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        version_handshake(&mut stdin, &client_sock, &mut stdout)
            .expect("handshake should succeed with matching version");
        handle.join().expect("daemon thread");

        assert!(!stdout.is_empty(), "response should be forwarded to stdout");
        let response: serde_json::Value =
            serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim())
                .expect("parse response");
        assert_eq!(response["result"]["serverInfo"]["name"], "catenary");
        assert_eq!(
            response["result"]["serverInfo"]["version"],
            env!("CATENARY_VERSION"),
        );
    }

    #[test]
    fn mismatched_version_rejected() {
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        let handle = fake_daemon(server_sock, "0.0.0-fake");

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        let result = version_handshake(&mut stdin, &client_sock, &mut stdout);
        handle.join().expect("daemon thread");

        assert!(result.is_err(), "handshake should fail on version mismatch");
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("version mismatch"),
            "error should mention mismatch: {err}",
        );
        assert!(
            err.contains("0.0.0-fake"),
            "error should contain daemon version: {err}",
        );
        assert!(stdout.is_empty(), "should not forward response on mismatch");
    }

    #[test]
    fn missing_version_rejected() {
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        // Daemon responds without a version field in serverInfo.
        let handle = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut reader = std::io::BufReader::new(&server_sock);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read init request");

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": { "name": "not-catenary" }
                }
            });
            let mut w: &std::os::unix::net::UnixStream = &server_sock;
            writeln!(w, "{}", serde_json::to_string(&response).expect("ser"))
                .expect("write response");
        });

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        let result = version_handshake(&mut stdin, &client_sock, &mut stdout);
        handle.join().expect("daemon thread");

        assert!(
            result.is_err(),
            "handshake should fail when version is missing"
        );
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("did not report a version"),
            "error should explain missing version: {err}",
        );
    }

    // ── Correlation tests ──────────────────────────────────────────

    /// Bind a `SessionManager` with both a session and echo handler
    /// for full correlation testing.
    fn bind_echo_with_session(dir: &Path) -> SessionManager {
        let db_path = dir.join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&db_path).expect("open test DB");
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('daemon', 1, 'test-daemon', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let logging = LoggingServer::new();
        let runtime = tokio::runtime::Handle::current();
        let instance_id: Arc<str> = "daemon".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default(),
            vec![],
            logging.clone(),
            conn.clone(),
            instance_id,
            runtime,
            notification_router,
        ));

        SessionManager::bind_at(
            &mcp_socket_in(dir),
            &hook_socket_in(dir),
            Arc::new(EchoHandler),
            logging,
        )
        .expect("bind")
        .with_session(session, conn)
    }

    /// Send a `PreToolUse` hook for a Catenary tool, then MCP `tools/call`
    /// on a connection. Returns the MCP stream (kept alive for the
    /// connection's lifetime).
    async fn correlate_session(
        hook_path: &Path,
        mcp_path: &Path,
        session_id: &str,
    ) -> std::os::unix::net::UnixStream {
        // 1. Send `PreToolUse` hook for a Catenary tool.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": session_id
        });
        let _ = hook_roundtrip(hook_path, &req).await;

        // 2. Connect MCP and send initialize + tools/call.
        let stream = std::os::unix::net::UnixStream::connect(mcp_path).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set timeout");

        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            }),
        );
        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "echo"}
            }),
        );

        stream
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn correlation_binds_connection_to_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = correlate_session(&hook_path, &mcp_path, "abc").await;

        // Allow the spawn_blocking MCP task to process tools/call.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(
            manager.session_has_connection("abc"),
            "session 'abc' should have a bound MCP connection",
        );

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn correlation_is_permanent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = correlate_session(&hook_path, &mcp_path, "abc").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Second tools/call on the same connection should not change binding.
        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "echo"}
            }),
        );

        assert!(
            manager.session_has_connection("abc"),
            "binding should remain 'abc' after second tools/call",
        );

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn already_resolved_skips_lock() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Correlate session "abc".
        let stream = correlate_session(&hook_path, &mcp_path, "abc").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Send another `PreToolUse` for the same session. It should not
        // create a new pending entry (fast path).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": "abc"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        assert!(
            !manager.has_pending_correlation(),
            "already-resolved session should not create a pending entry",
        );

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_sessions_serialize() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Correlate "abc" first, then "def".
        let s1 = correlate_session(&hook_path, &mcp_path, "abc").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let s2 = correlate_session(&hook_path, &mcp_path, "def").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(
            manager.session_has_connection("abc"),
            "session 'abc' should be correlated",
        );
        assert!(
            manager.session_has_connection("def"),
            "session 'def' should be correlated",
        );

        drop(s1);
        drop(s2);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_releases_lock() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send `PreToolUse` but do NOT send an MCP tools/call.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": "timeout-sess"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        assert!(
            manager.has_pending_correlation(),
            "should have a pending correlation entry",
        );

        // Since `CORRELATION_TIMEOUT` is 30s, we can't wait that long
        // in a test. Instead, verify the pending state exists, then
        // directly clear it (simulating the timeout task) to prove the
        // lock is released.
        *manager.correlation.pending.lock().expect("lock") = None;

        assert!(
            !manager.has_pending_correlation(),
            "pending entry should be cleared after timeout",
        );

        // Verify the semaphore is available (lock was released).
        let permit = manager.correlation.semaphore.try_acquire();
        assert!(
            permit.is_ok(),
            "serialization lock should be available after timeout",
        );
        drop(permit);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_session_re_correlates() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Simulate a timed-out correlation: send `PreToolUse`, then
        // manually clear the pending entry (simulating timeout).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_glob",
            "agent_id": "",
            "session_id": "re-corr"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        // Clear pending (simulates timeout clearing).
        *manager.correlation.pending.lock().expect("lock") = None;

        // Now re-correlate the same session.
        let stream = correlate_session(&hook_path, &mcp_path, "re-corr").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(
            manager.session_has_connection("re-corr"),
            "session should re-correlate after timeout",
        );

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_catenary_hook_no_correlation() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send `PreToolUse` for a non-Catenary tool (Bash).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Bash",
            "agent_id": "",
            "session_id": "no-corr"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;

        assert!(
            !manager.has_pending_correlation(),
            "non-Catenary tool should not trigger correlation",
        );

        shutdown.cancel();
    }

    // ── Root refcounting tests ────────────────────────────────────────

    #[test]
    fn single_session_adds_roots() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo"), PathBuf::from("/bar")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 2);
        assert!(global.contains(&PathBuf::from("/foo")));
        assert!(global.contains(&PathBuf::from("/bar")));
    }

    #[test]
    fn two_sessions_shared_root() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.set_roots("mcp:20", vec![PathBuf::from("/foo"), PathBuf::from("/bar")]);

        assert_eq!(tracker.refcount(Path::new("/foo")), 2);
        assert_eq!(tracker.refcount(Path::new("/bar")), 1);

        // Remove first session — /foo should survive (refcount 1).
        tracker.remove_contributor("mcp:10");

        let global = tracker.global_roots();
        assert!(
            global.contains(&PathBuf::from("/foo")),
            "/foo should survive"
        );
        assert!(
            global.contains(&PathBuf::from("/bar")),
            "/bar should survive"
        );
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);
    }

    #[test]
    fn last_session_removes_root() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.set_roots("mcp:20", vec![PathBuf::from("/foo")]);

        // Remove first — still has refcount 1.
        tracker.remove_contributor("mcp:10");
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);

        // Remove second — refcount 0, gone from global set.
        tracker.remove_contributor("mcp:20");
        assert_eq!(tracker.refcount(Path::new("/foo")), 0);
        assert!(tracker.global_roots().is_empty());
    }

    #[test]
    fn add_dir_increments_refcount() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        // Transcript scan adds a root for the same session.
        tracker.add_roots("transcript:sess-a", &[PathBuf::from("/bar")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 2);
        assert!(global.contains(&PathBuf::from("/foo")));
        assert!(global.contains(&PathBuf::from("/bar")));
    }

    #[test]
    fn duplicate_root_same_session_no_double_count() {
        let tracker = RootTracker::new();

        // Same contributor sets the same root via set_roots (idempotent).
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);

        // add_roots also deduplicates within the same contributor.
        tracker.add_roots("mcp:10", &[PathBuf::from("/foo")]);
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);
    }

    #[test]
    fn set_roots_replaces_previous() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo"), PathBuf::from("/bar")]);
        tracker.set_roots("mcp:10", vec![PathBuf::from("/baz")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 1);
        assert!(global.contains(&PathBuf::from("/baz")));
        assert!(!global.contains(&PathBuf::from("/foo")));
    }

    #[test]
    fn remove_nonexistent_contributor_is_noop() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.remove_contributor("mcp:99");

        assert_eq!(tracker.global_roots().len(), 1);
    }

    #[test]
    fn remove_root_from_hook_contributor() {
        let tracker = RootTracker::new();
        tracker.add_roots("hook", &[PathBuf::from("/foo"), PathBuf::from("/bar")]);

        assert!(
            tracker.remove_root("hook", Path::new("/foo")),
            "should return true when root was present",
        );
        let global = tracker.global_roots();
        assert_eq!(global.len(), 1);
        assert!(global.contains(&PathBuf::from("/bar")));
        assert!(!global.contains(&PathBuf::from("/foo")));
    }

    #[test]
    fn remove_root_last_entry_removes_contributor() {
        let tracker = RootTracker::new();
        tracker.add_roots("hook", &[PathBuf::from("/only")]);

        assert!(tracker.remove_root("hook", Path::new("/only")));
        assert!(
            tracker.global_roots().is_empty(),
            "global roots should be empty after removing last root",
        );
        // Verify the contributor key is fully removed.
        assert_eq!(
            tracker.refcount(Path::new("/only")),
            0,
            "refcount should be 0",
        );
    }

    #[test]
    fn remove_root_nonexistent_returns_false() {
        let tracker = RootTracker::new();
        tracker.add_roots("hook", &[PathBuf::from("/foo")]);

        assert!(
            !tracker.remove_root("hook", Path::new("/missing")),
            "should return false for missing root",
        );
        assert_eq!(tracker.global_roots().len(), 1);
    }

    #[test]
    fn remove_root_nonexistent_contributor_returns_false() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        assert!(
            !tracker.remove_root("hook", Path::new("/foo")),
            "should return false for nonexistent contributor",
        );
        assert_eq!(tracker.global_roots().len(), 1);
    }

    #[test]
    fn rm_root_removes_only_hook_roots() {
        let tracker = RootTracker::new();
        // Root is provided by both MCP and hook contributors.
        tracker.set_roots("mcp:10", vec![PathBuf::from("/shared")]);
        tracker.add_roots("hook", &[PathBuf::from("/shared")]);
        assert_eq!(tracker.refcount(Path::new("/shared")), 2);

        // rm-root removes only the hook entry.
        tracker.remove_root("hook", Path::new("/shared"));
        assert_eq!(
            tracker.refcount(Path::new("/shared")),
            1,
            "MCP contributor should still hold the root",
        );
        let global = tracker.global_roots();
        assert!(
            global.contains(&PathBuf::from("/shared")),
            "root should persist (MCP still holds it)",
        );
    }

    #[test]
    fn add_root_hook_contributor() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/existing")]);

        tracker.add_roots("hook", &[PathBuf::from("/new_root")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 2);
        assert!(global.contains(&PathBuf::from("/existing")));
        assert!(global.contains(&PathBuf::from("/new_root")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_cleanup_on_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Correlate a session.
        let stream = correlate_session(&hook_path, &mcp_path, "cleanup-sess").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(manager.session_count(), 1, "session should exist");
        assert!(
            manager.session_has_connection("cleanup-sess"),
            "should be correlated",
        );

        // Disconnect.
        drop(stream);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Session should be cleaned up.
        assert_eq!(
            manager.session_count(),
            0,
            "session should be removed after disconnect",
        );
        assert!(
            !manager.session_has_connection("cleanup-sess"),
            "correlation should be cleaned up",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_sessions_disconnect_one_preserves_other() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Correlate two sessions.
        let s1 = correlate_session(&hook_path, &mcp_path, "sess-keep").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let s2 = correlate_session(&hook_path, &mcp_path, "sess-drop").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(manager.session_count(), 2);

        // Disconnect one session.
        drop(s2);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            manager.session_count(),
            1,
            "only dropped session should be removed",
        );
        assert!(
            manager.session_has_connection("sess-keep"),
            "surviving session should still be correlated",
        );
        assert!(
            !manager.session_has_connection("sess-drop"),
            "dropped session should be cleaned up",
        );

        drop(s1);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_removes_root_scoped_roots() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Correlate a session.
        let stream = correlate_session(&hook_path, &mcp_path, "root-sess").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Look up the fd assigned to this connection.
        let fd = *manager
            .correlation
            .session_connections
            .lock()
            .expect("lock")
            .get("root-sess")
            .expect("should be correlated");

        // Set roots on the tracker for this connection (simulates
        // what on_roots_changed would do when roots/list arrives).
        let tracker = manager.root_tracker.as_ref().expect("tracker");
        let root = PathBuf::from("/test/project");
        tracker.set_roots(&format!("mcp:{fd}"), vec![root.clone()]);

        assert_eq!(tracker.refcount(&root), 1, "root should have refcount 1");

        // Disconnect — cleanup should remove this connection's roots.
        drop(stream);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            tracker.refcount(&root),
            0,
            "root should be removed after last session disconnects",
        );
        assert!(
            tracker.global_roots().is_empty(),
            "global roots should be empty",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shared_root_survives_partial_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Correlate two sessions.
        let s1 = correlate_session(&hook_path, &mcp_path, "ws-keep").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let s2 = correlate_session(&hook_path, &mcp_path, "ws-drop").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Look up fds.
        let fd_keep = *manager
            .correlation
            .session_connections
            .lock()
            .expect("lock")
            .get("ws-keep")
            .expect("should be correlated");
        let fd_drop = *manager
            .correlation
            .session_connections
            .lock()
            .expect("lock")
            .get("ws-drop")
            .expect("should be correlated");

        // Both sessions share /shared, second also has /exclusive.
        let tracker = manager.root_tracker.as_ref().expect("tracker");
        let shared = PathBuf::from("/shared/workspace");
        let exclusive = PathBuf::from("/exclusive/workspace");
        tracker.set_roots(&format!("mcp:{fd_keep}"), vec![shared.clone()]);
        tracker.set_roots(
            &format!("mcp:{fd_drop}"),
            vec![shared.clone(), exclusive.clone()],
        );

        assert_eq!(tracker.refcount(&shared), 2);
        assert_eq!(tracker.refcount(&exclusive), 1);

        // Disconnect second session — /shared should survive, /exclusive should go.
        drop(s2);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            tracker.refcount(&shared),
            1,
            "/shared should still have refcount 1 from surviving session",
        );
        assert_eq!(
            tracker.refcount(&exclusive),
            0,
            "/exclusive should be removed (only contributor disconnected)",
        );
        let global = tracker.global_roots();
        assert!(
            global.contains(&shared),
            "/shared should be in global roots",
        );
        assert!(
            !global.contains(&exclusive),
            "/exclusive should not be in global roots",
        );

        drop(s1);
        shutdown.cancel();
    }

    // ── Function-level tests (mutant audit 03-07) ─────────────────

    /// `mcp_socket_path` returns a deterministic path inside `state_dir`.
    #[test]
    fn test_mcp_socket_path_structure() {
        let path = mcp_socket_path();
        assert!(
            path.ends_with("catenary/catenary.sock"),
            "mcp_socket_path should end with catenary/catenary.sock, got: {}",
            path.display()
        );
    }

    /// `is_correlation_trigger` returns true for Catenary `PreToolUse` hooks.
    #[test]
    fn test_is_correlation_trigger_catenary_tools() {
        // Direct tool names
        let pre = |name: &str| {
            let raw = serde_json::json!({"tool_name": name});
            is_correlation_trigger("pre-tool/editing-state", &raw)
        };

        assert!(pre("grep"), "grep should trigger");
        assert!(pre("glob"), "glob should trigger");
        assert!(pre("start_editing"), "start_editing should trigger");
        assert!(pre("done_editing"), "done_editing should trigger");

        // MCP-qualified names
        assert!(pre("mcp_catenary_grep"), "mcp_catenary_grep should trigger");
        assert!(
            pre("mcp__plugin_catenary_catenary__glob"),
            "mcp qualified glob should trigger"
        );
    }

    /// `is_correlation_trigger` returns false for non-Catenary tools.
    #[test]
    fn test_is_correlation_trigger_non_catenary() {
        let raw = serde_json::json!({"tool_name": "Edit"});
        assert!(
            !is_correlation_trigger("pre-tool/editing-state", &raw),
            "non-Catenary tool should not trigger",
        );
    }

    /// `is_correlation_trigger` returns false for non-PreToolUse methods.
    #[test]
    fn test_is_correlation_trigger_wrong_method() {
        let raw = serde_json::json!({"tool_name": "grep"});
        assert!(
            !is_correlation_trigger("post-tool/editing-state", &raw),
            "PostToolUse should not trigger"
        );
        assert!(
            !is_correlation_trigger("pre-agent/turn-start", &raw),
            "PreAgent should not trigger"
        );
    }

    /// `parse_root_uris` extracts canonical paths from file:// URIs.
    #[test]
    fn test_parse_root_uris_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_path = dir.path().to_path_buf();
        let canonical = root_path.canonicalize().expect("canonicalize");
        let uri = format!("file://{}", root_path.display());

        let roots = vec![crate::mcp::Root {
            uri,
            name: Some("test".to_string()),
        }];
        let result = parse_root_uris(&roots);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], canonical);
    }

    /// `parse_root_uris` skips non-file:// URIs.
    #[test]
    fn test_parse_root_uris_non_file() {
        let roots = vec![crate::mcp::Root {
            uri: "https://example.com".to_string(),
            name: Some("remote".to_string()),
        }];
        let result = parse_root_uris(&roots);
        assert!(result.is_empty(), "non-file URIs should be skipped");
    }

    /// `parse_root_uris` skips paths that fail to canonicalize.
    #[test]
    fn test_parse_root_uris_nonexistent() {
        let roots = vec![crate::mcp::Root {
            uri: "file:///nonexistent/path/that/does/not/exist".to_string(),
            name: None,
        }];
        let result = parse_root_uris(&roots);
        assert!(result.is_empty(), "nonexistent paths should be skipped");
    }

    /// `session_for_fd` returns `None` when no correlation exists.
    #[tokio::test]
    async fn test_session_for_fd_uncorrelated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = bind_in(dir.path());

        assert!(
            manager.session_for_fd(42).is_none(),
            "uncorrelated fd should return None"
        );
    }

    /// `session_for_fd` returns the session ID after correlation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_session_for_fd_after_correlation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = correlate_session(&hook_path, &mcp_path, "test-sess").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Find the fd that was correlated
        let corr_sessions = manager
            .correlation
            .connection_sessions
            .lock()
            .expect("lock");
        let (&fd, session_id) = corr_sessions
            .iter()
            .next()
            .expect("should have a correlated fd");
        assert_eq!(session_id, "test-sess");
        drop(corr_sessions);

        // session_for_fd should return the session ID
        let result = manager.session_for_fd(fd);
        assert_eq!(
            result.as_deref(),
            Some("test-sess"),
            "session_for_fd should return the correlated session"
        );

        drop(stream);
        shutdown.cancel();
    }

    /// `CorrelatingHandler::list_tools` delegates to the inner handler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_correlating_handler_list_tools_delegates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set timeout");

        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            }),
        );

        let response = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }),
        );

        let tools = response["result"]["tools"].as_array().expect("tools array");
        assert!(
            !tools.is_empty(),
            "list_tools should delegate to inner handler, not return empty"
        );
        assert_eq!(tools[0]["name"], "echo", "should contain echo tool");

        drop(stream);
        shutdown.cancel();
    }

    /// Correlation creates a pending entry that is consumed on `tools/call`.
    ///
    /// Verifies the full flow: `PreToolUse` creates pending, `tools/call`
    /// resolves it and binds the connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_correlation_pending_consumed_on_tools_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hook_path = hook_socket_in(dir.path());
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send PreToolUse for a Catenary tool — creates pending entry.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": "pending-test"
        });
        let _ = hook_roundtrip(&hook_path, &req).await;
        assert!(
            manager.has_pending_correlation(),
            "pending entry should exist after PreToolUse"
        );

        // MCP connect + tools/call resolves the pending entry.
        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set timeout");
        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            }),
        );
        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": {"name": "echo"}
            }),
        );

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !manager.has_pending_correlation(),
            "pending entry should be consumed after tools/call"
        );
        assert!(
            manager.session_has_connection("pending-test"),
            "session should be bound"
        );

        drop(stream);
        shutdown.cancel();
    }
}
