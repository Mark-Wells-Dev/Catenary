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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};

use crate::bridge::EditingGuardrail;
use crate::bridge::HookRouter;
use crate::bridge::session::Session;
use crate::hook::{HookRequest, HookResponseEnvelope, emit_hook_event, hook_outcome_level};
use crate::logging::LoggingServer;
use crate::mcp::McpServer;
use crate::source::Source;

/// Returns the MCP socket path for the daemon.
///
/// The path is deterministic: `$XDG_STATE_HOME/catenary/catenary-mcp.sock`
/// (or platform equivalent via [`crate::db::state_dir`]).
///
/// Only the bridge proxy connects to this socket — it carries MCP
/// JSON-RPC traffic between the host CLI and the daemon.
#[must_use]
pub fn mcp_socket_path() -> PathBuf {
    crate::db::state_dir()
        .join("catenary")
        .join("catenary-mcp.sock")
}

/// Returns the general-purpose IPC socket path for the daemon.
///
/// The path is deterministic: `$XDG_STATE_HOME/catenary/catenary.sock`
/// (or platform equivalent via [`crate::db::state_dir`]).
///
/// This socket carries all non-MCP daemon traffic: hook events
/// (`pre-tool/*`, `post-agent/*`, etc.) and CLI commands
/// (`editing-start`, `editing-stop`, `roots-add`, `roots-rm`,
/// `roots-ls`, `shutdown`).
#[must_use]
pub fn socket_path() -> PathBuf {
    crate::db::state_dir()
        .join("catenary")
        .join("catenary.sock")
}

// ── IPC request/response types for CLI tool commands ─────────────

/// IPC method string for grep requests.
pub const METHOD_GREP: &str = "tool/grep";

/// IPC method string for glob requests.
pub const METHOD_GLOB: &str = "tool/glob";

/// IPC request payload for `catenary grep`.
///
/// Sent as a JSON line over the daemon IPC socket with
/// `"method": "tool/grep"`. [`to_params`](Self::to_params) resolves
/// relative paths and `exclude` patterns against `cwd` before
/// dispatching to the grep pipeline.
///
/// Wire format:
/// ```json
/// {"method": "tool/grep", "cwd": "/path", "pattern": "foo", "paths": ["src/main.rs"]}
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GrepRequest {
    /// Working directory from the CLI process.
    ///
    /// `None` when the caller has no meaningful cwd (e.g. test fixtures
    /// using `spawn_in_state`). When absent, the daemon falls back to
    /// searching all workspace roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Search pattern (regex, supports `|` for alternation).
    pub pattern: String,
    /// Literal file/directory paths to scope the search.
    ///
    /// All positional arguments are concrete filesystem paths — the
    /// shell is the only glob engine. These bypass glob matching and
    /// are used as direct search roots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathBuf>,
    /// Glob pattern to exclude from matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
    /// Page number for paged results (1-based, default: 1).
    #[serde(default = "ipc_default_page")]
    pub page: usize,
    /// Include files ignored by `.gitignore`.
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden files and directories.
    #[serde(default)]
    pub include_hidden: bool,
}

impl GrepRequest {
    /// Resolves relative paths against `cwd` and produces a
    /// `GrepInput`-compatible JSON value for the grep pipeline.
    ///
    /// - Paths are resolved against `cwd` (relative → absolute).
    /// - `exclude` is resolved against `cwd`.
    /// - `targets_hidden` is checked on paths to auto-enable
    ///   `include_hidden` for explicit hidden targets like `.gitignore`.
    fn to_params(&self) -> serde_json::Value {
        let mut include_hidden = self.include_hidden;

        let mut params = serde_json::json!({
            "pattern": self.pattern,
            "page": self.page,
            "include_gitignored": self.include_gitignored,
        });

        if self.paths.is_empty() {
            // No paths — cwd-scoped search. Pass cwd so the daemon
            // scopes to the agent's working directory.
            if let Some(ref cwd) = self.cwd {
                params["cwd"] = serde_json::Value::String(cwd.to_string_lossy().into_owned());
            }
        } else {
            // Literal paths — resolve relative paths against cwd,
            // check for hidden targeting.
            for p in &self.paths {
                let s = p.to_string_lossy();
                if !p.is_absolute() && crate::bridge::session::ResolvedGlob::targets_hidden(&s) {
                    include_hidden = true;
                }
            }
            params["paths"] = serde_json::Value::Array(
                self.paths
                    .iter()
                    .map(|p| {
                        let s = if p.is_absolute() {
                            p.to_string_lossy().into_owned()
                        } else {
                            self.cwd.as_ref().map_or_else(
                                || p.to_string_lossy().into_owned(),
                                |cwd| cwd.join(p).to_string_lossy().into_owned(),
                            )
                        };
                        serde_json::Value::String(s)
                    })
                    .collect(),
            );
        }
        if let Some(ref exclude) = self.exclude {
            let resolved = self
                .cwd
                .as_ref()
                .map_or_else(|| exclude.clone(), |cwd| resolve_relative(exclude, cwd));
            params["exclude"] = serde_json::Value::String(resolved);
        }
        params["include_hidden"] = serde_json::Value::Bool(include_hidden);
        params
    }
}

/// IPC response for `catenary grep`.
///
/// Returned as a JSON line over the daemon IPC socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GrepResponse {
    /// Rendered grep output.
    pub output: String,
}

/// IPC request payload for `catenary glob`.
///
/// Sent as a JSON line over the daemon IPC socket with
/// `"method": "tool/glob"`. The daemon resolves relative paths
/// against `cwd` before dispatching to the glob pipeline.
///
/// Wire format:
/// ```json
/// {"method": "tool/glob", "cwd": "/path", "paths": ["src/"]}
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobRequest {
    /// Working directory from the CLI process.
    ///
    /// `None` when the caller has no meaningful cwd. When absent, the
    /// daemon falls back to searching all workspace roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Literal file/directory paths.
    ///
    /// All positional arguments are concrete filesystem paths — the
    /// shell is the only glob engine. Each is dispatched through the
    /// appropriate handler (file outline, directory listing).
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Glob pattern to exclude from results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
    /// Page number for paged results (1-based, default: 1).
    #[serde(default = "ipc_default_page")]
    pub page: usize,
    /// Include files ignored by `.gitignore`.
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden files and directories.
    #[serde(default)]
    pub include_hidden: bool,
}

impl GlobRequest {
    /// Resolves relative paths against `cwd` and produces a
    /// `GlobInput`-compatible JSON value for the glob pipeline.
    ///
    /// - Paths are resolved against `cwd` (relative → absolute).
    /// - `targets_hidden` is checked on paths to auto-enable
    ///   `include_hidden` for explicit hidden targets.
    /// - Basename `exclude` patterns (no `/`) get a `**/` prefix for
    ///   depth-independent matching; patterns with `/` are resolved
    ///   against `cwd`.
    fn to_params(&self) -> serde_json::Value {
        let mut include_hidden = self.include_hidden;

        let mut params = serde_json::json!({
            "page": self.page,
            "include_gitignored": self.include_gitignored,
        });

        // Check for hidden targeting on relative paths.
        for p in &self.paths {
            let s = p.to_string_lossy();
            if !p.is_absolute() && crate::bridge::session::ResolvedGlob::targets_hidden(&s) {
                include_hidden = true;
            }
        }

        // Resolve relative paths against cwd.
        params["paths"] = serde_json::Value::Array(
            self.paths
                .iter()
                .map(|p| {
                    let s = if p.is_absolute() {
                        p.to_string_lossy().into_owned()
                    } else {
                        self.cwd.as_ref().map_or_else(
                            || p.to_string_lossy().into_owned(),
                            |cwd| cwd.join(p).to_string_lossy().into_owned(),
                        )
                    };
                    serde_json::Value::String(s)
                })
                .collect(),
        );
        params["include_hidden"] = serde_json::Value::Bool(include_hidden);

        if let Some(ref exclude) = self.exclude {
            let effective = if exclude.contains('/') {
                self.cwd
                    .as_ref()
                    .map_or_else(|| exclude.clone(), |cwd| resolve_relative(exclude, cwd))
            } else {
                format!("**/{exclude}")
            };
            params["exclude"] = serde_json::Value::String(effective);
        }
        params
    }
}

/// IPC response for `catenary glob`.
///
/// Returned as a JSON line over the daemon IPC socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobResponse {
    /// Rendered glob output.
    pub output: String,
}

/// Default page number for IPC tool requests (1-based).
const fn ipc_default_page() -> usize {
    1
}

/// Resolves a pattern path against a base directory if it is relative.
///
/// Tilde-expands the pattern first. Absolute paths and `~` paths are
/// returned as-is. Relative paths are joined to `base`.
fn resolve_relative(pattern: &str, base: &Path) -> String {
    let expanded = crate::bridge::expand_tilde(pattern);
    if Path::new(&expanded).is_absolute() {
        return expanded;
    }
    base.join(&expanded).to_string_lossy().into_owned()
}

/// Pre-bound MCP and IPC socket listeners.
///
/// Returned by [`bind_daemon_sockets`] for early socket binding in daemon
/// mode. Pass to [`SessionManager::from_listeners`] once the tool handler
/// is ready.
#[cfg(unix)]
pub struct DaemonSockets {
    /// MCP socket listener.
    pub mcp_listener: tokio::net::UnixListener,
    /// General-purpose IPC socket listener.
    pub ipc_listener: tokio::net::UnixListener,
    /// Filesystem path of the MCP socket.
    pub mcp_path: PathBuf,
    /// Filesystem path of the IPC socket.
    pub ipc_path: PathBuf,
}

/// Binds the daemon's MCP and IPC sockets immediately.
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
    let ipc_path = socket_path();

    if let Some(parent) = mcp_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory: {}", parent.display()))?;
    }
    if let Some(parent) = ipc_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory: {}", parent.display()))?;
    }

    let mcp_listener = tokio::net::UnixListener::bind(&mcp_path)
        .with_context(|| format!("bind MCP socket: {}", mcp_path.display()))?;
    let ipc_listener = tokio::net::UnixListener::bind(&ipc_path)
        .with_context(|| format!("bind IPC socket: {}", ipc_path.display()))?;

    info!(
        source = Source::DaemonLifecycle.as_str(),
        mcp_path = %mcp_path.display(),
        ipc_path = %ipc_path.display(),
        "daemon sockets bound",
    );

    Ok(DaemonSockets {
        mcp_listener,
        ipc_listener,
        mcp_path,
        ipc_path,
    })
}

// ── Session registry ───────────────────────────────────────────────

/// Per-session state: the [`HookRouter`] (which owns the `Session`).
#[cfg(unix)]
struct SessionEntry {
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
    /// Logging server for sink access.
    _logging: LoggingServer,
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

/// Handoff context deposited by `pre-tool/editing-stop`
/// and consumed by `tool/editing-stop`.
///
/// Dropping this struct drops the owned semaphore permit, releasing
/// the handoff lock.
struct HandoffContext {
    /// Accumulated files from the editing session.
    files: Vec<PathBuf>,
    /// Number of files skipped because they were outside tracked
    /// workspace roots (no LSP coverage).
    filtered: usize,
    /// Scope UUID minted at prepare time. Used as `parent_id` for the
    /// IPC request/response events and all LSP children from
    /// `process_files_batched`, linking them into one TUI scope.
    parent_id: String,
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
                "editing stop handoff timeout — discarding file list",
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
/// have their per-root server instances shut down.
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

    /// Returns all roots with their contributor sources.
    ///
    /// Each entry is `(path, sources)` where `sources` is a sorted list
    /// of contributor keys (e.g., `["hook", "mcp:3"]`).
    fn list_roots(&self) -> Vec<(PathBuf, Vec<String>)> {
        let map = self
            .contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Invert: root → list of contributors.
        let mut root_sources: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for (contributor, roots) in &*map {
            for root in roots {
                root_sources
                    .entry(root.clone())
                    .or_default()
                    .push(contributor.clone());
            }
        }
        drop(map);

        let mut result: Vec<(PathBuf, Vec<String>)> = root_sources.into_iter().collect();
        result.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (_, sources) in &mut result {
            sources.sort();
        }
        result
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
/// with a protocol-only `McpServer` (roots, lifecycle). Hook connections
/// are routed to per-`session_id` [`HookRouter`] instances when a shared
/// [`Session`] is configured (daemon mode), or receive passthrough
/// responses (test mode).
#[cfg(unix)]
pub struct SessionManager {
    mcp_listener: tokio::net::UnixListener,
    ipc_listener: tokio::net::UnixListener,
    mcp_socket_path: PathBuf,
    ipc_socket_path: PathBuf,
    logging: LoggingServer,
    connection_count: Arc<AtomicUsize>,
    /// Monotonic counter for unique MCP connection IDs. Incremented
    /// once per accepted connection; never decremented. Used as the
    /// session key (`mcp:{n}`) to avoid fd-reuse collisions.
    next_connection_id: Arc<AtomicUsize>,
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
    shutdown: CancellationToken,
    disconnect: Arc<tokio::sync::Notify>,
}

#[cfg(unix)]
impl SessionManager {
    /// Binds the MCP and IPC sockets at the default paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory cannot be created or
    /// either socket cannot be bound (e.g., another daemon is already
    /// running).
    pub fn bind(logging: LoggingServer) -> Result<Self> {
        Self::bind_at(&mcp_socket_path(), &socket_path(), logging)
    }

    /// Creates a `SessionManager` from pre-bound sockets.
    ///
    /// Consumes the [`DaemonSockets`] returned by [`bind_daemon_sockets`],
    /// transferring socket ownership. Used in daemon mode where sockets
    /// are bound before heavy initialization so bridges can connect
    /// immediately. [`SessionManager::drop`] cleans up the socket files.
    #[must_use]
    pub fn from_sockets(sockets: DaemonSockets, logging: LoggingServer) -> Self {
        Self {
            mcp_listener: sockets.mcp_listener,
            ipc_listener: sockets.ipc_listener,
            mcp_socket_path: sockets.mcp_path,
            ipc_socket_path: sockets.ipc_path,
            logging,
            connection_count: Arc::new(AtomicUsize::new(0)),
            next_connection_id: Arc::new(AtomicUsize::new(0)),
            hook_ctx: None,
            lsp: None,
            root_tracker: None,
            db_conn: None,
            shutdown: CancellationToken::new(),
            disconnect: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Binds the MCP and IPC sockets at explicit paths.
    ///
    /// Used by tests to isolate socket files in tempdirs.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directories cannot be created or
    /// either socket cannot be bound.
    pub fn bind_at(mcp_path: &Path, ipc_path: &Path, logging: LoggingServer) -> Result<Self> {
        if let Some(parent) = mcp_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory: {}", parent.display()))?;
        }
        if let Some(parent) = ipc_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory: {}", parent.display()))?;
        }

        let mcp_listener = tokio::net::UnixListener::bind(mcp_path)
            .with_context(|| format!("bind MCP socket: {}", mcp_path.display()))?;
        let ipc_listener = tokio::net::UnixListener::bind(ipc_path)
            .with_context(|| format!("bind IPC socket: {}", ipc_path.display()))?;

        info!(
            source = Source::DaemonLifecycle.as_str(),
            mcp_path = %mcp_path.display(),
            ipc_path = %ipc_path.display(),
            "daemon started",
        );

        Ok(Self {
            mcp_listener,
            ipc_listener,
            mcp_socket_path: mcp_path.to_path_buf(),
            ipc_socket_path: ipc_path.to_path_buf(),
            logging,
            connection_count: Arc::new(AtomicUsize::new(0)),
            next_connection_id: Arc::new(AtomicUsize::new(0)),
            hook_ctx: None,
            lsp: None,
            root_tracker: None,
            db_conn: None,
            shutdown: CancellationToken::new(),
            disconnect: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Accepts incoming MCP and IPC connections in a loop.
    ///
    /// Each MCP connection spawns a per-connection async task with a
    /// `McpServer` (protocol-only, no tools). The task runs in a tracing
    /// span tagged with `mcp_fd` for log correlation. IPC connections
    /// are short-lived and handled in spawned tasks with passthrough
    /// responses.
    ///
    /// Returns `Ok(())` when the daemon should shut down. Three triggers:
    /// - Last MCP client disconnected (disconnect notify, count == 0)
    /// - `catenary stop` received on the IPC socket (shutdown token)
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
                result = self.ipc_listener.accept() => {
                    let (stream, _addr) = result.context("accept IPC connection")?;
                    let shutdown = self.shutdown.clone();
                    if let Some(ctx) = &self.hook_ctx {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_hook_dispatch(stream, ctx, shutdown).await {
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
        let _ = std::fs::remove_file(&self.ipc_socket_path);
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
        let logging = self.logging.clone();
        let count = Arc::clone(&self.connection_count);
        let disconnect = Arc::clone(&self.disconnect);
        let lsp = self.lsp.clone();
        let primary_session = self.hook_ctx.as_ref().map(|ctx| ctx.primary.clone());
        let root_tracker = self.root_tracker.clone();
        let db_conn = self.db_conn.clone();

        // Per-connection session key. Monotonic counter avoids
        // collisions from fd reuse across the daemon's lifetime.
        // The `mcp:` prefix distinguishes connection sessions from
        // hook sessions (which remain for internal routing only).
        let conn_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let session_key = format!("mcp:{conn_id}");

        // Create the per-connection session row before spawning so the
        // FK constraint (messages.session_id → sessions.id) is
        // satisfied for events emitted inside the connection span.
        if let Some(ref conn) = db_conn
            && let Ok(c) = conn.lock()
        {
            let started_at = chrono::Utc::now().to_rfc3339();
            let _ = c.execute(
                "INSERT INTO sessions \
                 (id, pid, display_name, started_at, alive) \
                 VALUES (?1, ?2, ?3, ?4, 1) \
                 ON CONFLICT(id) DO UPDATE SET \
                   alive = 1, \
                   display_name = excluded.display_name, \
                   started_at = excluded.started_at, \
                   ended_at = NULL",
                rusqlite::params![&session_key, std::process::id(), "", &started_at,],
            );
        }

        // Clone DB connection for disconnect cleanup (originals move
        // into spawn_blocking for callback closures).
        let db_conn_cleanup = db_conn.clone();

        count.fetch_add(1, Ordering::Relaxed);

        tokio::spawn(async move {
            let span = tracing::info_span!(
                "mcp_connection",
                mcp_fd = fd,
                session_id = %session_key,
            );
            let span_for_blocking = span.clone();
            let session_key_cleanup = session_key.clone();
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
                let session_cleanup = primary_session.clone();
                let lsp_cleanup = lsp.clone();

                let result = tokio::task::spawn_blocking(move || {
                    let _entered = span_for_blocking.enter();

                    let mut mcp = McpServer::new(logging);

                    // Wire lifecycle callbacks when the shared LSP
                    // infrastructure is available (daemon mode). When a
                    // root tracker is configured, root changes go through
                    // refcounting so multiple sessions can share roots
                    // without clobbering each other.
                    match (root_tracker, lsp, primary_session) {
                        (Some(tracker), Some(_), Some(session)) => {
                            let mcp_key = format!("mcp:{fd}");
                            let db_for_roots = db_conn.clone();
                            let key_for_roots = session_key.clone();
                            mcp = mcp.on_roots_changed(Box::new(move |roots| {
                                let paths = parse_root_uris(&roots);
                                // Update display_name on the per-connection
                                // session row so the TUI sidebar shows the
                                // workspace path(s).
                                if let Some(ref conn) = db_for_roots
                                    && let Ok(c) = conn.lock()
                                {
                                    let display = paths
                                        .iter()
                                        .map(|p| p.to_string_lossy().into_owned())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let _ = c.execute(
                                        "UPDATE sessions SET display_name = ?1 \
                                         WHERE id = ?2",
                                        rusqlite::params![&display, &key_for_roots],
                                    );
                                }
                                tracker.set_roots(&mcp_key, paths);
                                let global = tracker.global_roots();
                                tokio::runtime::Handle::current()
                                    .block_on(session.sync_roots(global))?;
                                Ok(())
                            }));
                        }
                        (None, Some(cm), _) => {
                            let db_for_roots = db_conn.clone();
                            let key_for_roots = session_key.clone();
                            mcp = mcp.on_roots_changed(Box::new(move |roots| {
                                let paths = parse_root_uris(&roots);
                                if let Some(ref conn) = db_for_roots
                                    && let Ok(c) = conn.lock()
                                {
                                    let display = paths
                                        .iter()
                                        .map(|p| p.to_string_lossy().into_owned())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let _ = c.execute(
                                        "UPDATE sessions SET display_name = ?1 \
                                         WHERE id = ?2",
                                        rusqlite::params![&display, &key_for_roots],
                                    );
                                }
                                tokio::runtime::Handle::current().block_on(cm.sync_roots(paths))?;
                                Ok(())
                            }));
                        }
                        _ => {}
                    }

                    if let Some(conn) = db_conn {
                        let key = session_key;
                        mcp = mcp.on_client_info(Box::new(move |name: &str, version: &str| {
                            if let Ok(c) = conn.lock() {
                                let _ = c.execute(
                                    "UPDATE sessions SET client_name = ?1, \
                                     client_version = ?2 WHERE id = ?3",
                                    rusqlite::params![name, version, &key],
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
                // Mark the per-connection session dead so the TUI
                // drops it from the sidebar. Remove roots from the
                // tracker and sync the reduced root set to LSP servers.
                if let Some(ref conn) = db_conn_cleanup
                    && let Ok(c) = conn.lock()
                {
                    let ended_at = chrono::Utc::now().to_rfc3339();
                    let _ = c.execute(
                        "UPDATE sessions SET alive = 0, ended_at = ?1 \
                         WHERE id = ?2",
                        rusqlite::params![&ended_at, &session_key_cleanup],
                    );
                }

                if let Some(ref tracker) = tracker_cleanup {
                    let mcp_key = format!("mcp:{fd}");
                    tracker.remove_contributor(&mcp_key);

                    // Sync the reduced root set through the primary
                    // session so both FilesystemManager and PathValidator
                    // are updated.
                    let global = tracker.global_roots();
                    let sync_result = if let Some(ref session) = session_cleanup {
                        session.sync_roots(global).await
                    } else if let Some(ref cm) = lsp_cleanup {
                        cm.sync_roots(global).await
                    } else {
                        Ok(())
                    };
                    if let Err(e) = sync_result {
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            "root sync after disconnect failed: {e}",
                        );
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

    /// Returns the IPC socket path this manager is bound to.
    #[must_use]
    pub fn ipc_path(&self) -> &Path {
        &self.ipc_socket_path
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
            _logging: self.logging.clone(),
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
///
/// Also inserts a row into the `sessions` table on first creation so the
/// TUI sidebar can discover per-agent sessions.
#[cfg(unix)]
fn get_or_create_router(
    ctx: &HookDispatchContext,
    session_id: &str,
    raw: &serde_json::Value,
) -> Arc<HookRouter> {
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

            // Insert a session row so the TUI can discover this agent.
            // Uses the cwd from the host payload as display_name, and
            // the format field as client_name (for sidebar host label).
            let display_name = raw
                .get("host_payload")
                .and_then(|hp| hp.get("cwd"))
                .and_then(|v| v.as_str())
                .unwrap_or(session_id);
            let client_name = raw.get("format").and_then(|v| v.as_str());
            if let Ok(conn) = ctx.conn.lock() {
                let started_at = chrono::Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "INSERT INTO sessions \
                     (id, pid, display_name, client_name, started_at, alive) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 1) \
                     ON CONFLICT(id) DO UPDATE SET \
                       alive = 1, \
                       display_name = excluded.display_name, \
                       client_name = COALESCE(excluded.client_name, sessions.client_name), \
                       started_at = excluded.started_at, \
                       ended_at = NULL",
                    rusqlite::params![
                        session_id,
                        std::process::id(),
                        display_name,
                        client_name,
                        &started_at,
                    ],
                );
            }

            let router = Arc::new(HookRouter::new(
                session.clone(),
                ctx.conn.clone(),
                session.instance_id.clone(),
                session_id.to_string(),
            ));
            SessionEntry { router }
        })
        .router
        .clone()
}

/// Handles a single hook connection with session-aware dispatch.
///
/// Reads the JSON request, extracts `session_id` for routing, looks up
/// (or creates) the per-session [`HookRouter`], dispatches the request,
/// logs the protocol pair, and writes the response.
#[cfg(unix)]
#[allow(clippy::too_many_lines, reason = "sequential protocol steps")]
async fn handle_hook_dispatch(
    stream: tokio::net::UnixStream,
    ctx: HookDispatchContext,
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

    // ── List tracked roots ─────────────────────────────────────
    //
    // `tool/roots-ls` is sent by `catenary roots ls`. Returns all
    // tracked workspace roots with their contributor sources.
    if method == "tool/roots-ls" {
        let roots = ctx
            .root_tracker
            .as_ref()
            .map_or_else(Vec::new, RootTracker::list_roots);

        let roots_json: Vec<serde_json::Value> = roots
            .into_iter()
            .map(|(path, sources)| {
                serde_json::json!({
                    "path": path.display().to_string(),
                    "sources": sources,
                })
            })
            .collect();

        let response = serde_json::json!({
            "status": "ok",
            "roots": roots_json,
        });

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // Extract session_id for routing. Falls back to "default" for hooks
    // that don't carry a session_id (backward compatibility).
    let session_id = raw
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    // ── Session-end cleanup ───────────────────────────────────────
    //
    // Fires when the host CLI sends a SessionEnd hook (exit, /clear,
    // resume, logout). Cleans up session-scoped state: editing
    // guardrail, notification router, session registry, and roots.
    //
    // Short-circuits before get_or_create_router to avoid creating
    // a new session just to immediately clean it up.
    if method == "session-end/cleanup" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        // Release editing guardrail locks (idempotent if MCP
        // disconnect already ran).
        ctx.editing_guardrail.release_all(&session_id);

        // Remove session from notification router (idempotent if
        // MCP disconnect already ran).
        ctx.primary.notification_router.remove_session(&session_id);

        // Remove the session from the registry.
        ctx.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session_id);

        // Mark session dead in DB so the TUI drops it from the sidebar.
        if let Ok(conn) = ctx.conn.lock() {
            let ended_at = chrono::Utc::now().to_rfc3339();
            let _ = conn.execute(
                "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                rusqlite::params![&ended_at, &session_id],
            );
        }

        if let Some(ref tracker) = ctx.root_tracker {
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
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "",
            "outgoing hook response",
        );

        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Start editing confirmation ────────────────────────────────
    //
    // `tool/editing-start` is sent by `catenary editing start`
    // after the PreToolUse hook has already entered editing mode.
    // The CLI command just needs a confirmation response.
    if method == "tool/editing-start" {
        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Grep query ──────────────────────────────────────────────
    //
    // `tool/grep` is sent by `catenary grep`. Resolves relative
    // patterns against `cwd`, dispatches to the grep pipeline, and
    // returns the rendered output as a `GrepResponse`.
    if method == METHOD_GREP {
        let grep_req: GrepRequest = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("invalid grep request: {e}"))?;

        let params = grep_req.to_params();
        let parent_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&parent_id),
            &raw.to_string(),
            "incoming hook",
        );

        // Race grep execution against client disconnect so a killed
        // CLI process doesn't leave the pipeline running indefinitely.
        let cancel_on_disconnect = cancel.clone();
        let output = tokio::select! {
            result = ctx.primary.grep.execute(&params, Some(&parent_id), &cancel) => {
                match result {
                    Ok(v) => v.as_str().unwrap_or("").to_string(),
                    Err(e) => format!("grep error: {e}"),
                }
            }
            () = async {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 1];
                let _ = buf_reader.read(&mut buf).await;
                cancel_on_disconnect.cancel();
            } => {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    "grep client disconnected — query cancelled",
                );
                emit_hook_event(
                    tracing::Level::INFO,
                    "cli",
                    &method,
                    Some(&parent_id),
                    "client disconnected",
                    "outgoing hook response",
                );
                return Ok(());
            }
        };

        let response = GrepResponse { output };
        let mut payload = serde_json::to_vec(&response)?;

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&parent_id),
            std::str::from_utf8(&payload).unwrap_or_default(),
            "outgoing hook response",
        );

        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Glob query ──────────────────────────────────────────────
    //
    // `tool/glob` is sent by `catenary glob`. Resolves relative
    // patterns against `cwd`, dispatches to the glob pipeline, and
    // returns the rendered output as a `GlobResponse`.
    if method == METHOD_GLOB {
        let glob_req: GlobRequest = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("invalid glob request: {e}"))?;

        let params = glob_req.to_params();
        let parent_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&parent_id),
            &raw.to_string(),
            "incoming hook",
        );

        // Race glob execution against client disconnect so a killed
        // CLI process doesn't leave the pipeline running indefinitely.
        let cancel_on_disconnect = cancel.clone();
        let output = tokio::select! {
            result = ctx.primary.glob.execute(&params, Some(&parent_id), &cancel) => {
                match result {
                    Ok(v) => v.as_str().unwrap_or("").to_string(),
                    Err(e) => format!("glob error: {e}"),
                }
            }
            () = async {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 1];
                let _ = buf_reader.read(&mut buf).await;
                cancel_on_disconnect.cancel();
            } => {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    "glob client disconnected — query cancelled",
                );
                emit_hook_event(
                    tracing::Level::INFO,
                    "cli",
                    &method,
                    Some(&parent_id),
                    "client disconnected",
                    "outgoing hook response",
                );
                return Ok(());
            }
        };

        let response = GlobResponse { output };
        let mut payload = serde_json::to_vec(&response)?;

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&parent_id),
            std::str::from_utf8(&payload).unwrap_or_default(),
            "outgoing hook response",
        );

        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Done editing handoff: prepare ────────────────────────────
    //
    // `pre-tool/editing-stop` is sent by the PreToolUse hook when
    // the agent runs `catenary editing stop`. Acquires
    // the handoff lock, drains files, releases the editing guardrail,
    // and deposits the file list for the subsequent CLI command.
    if method == "pre-tool/editing-stop" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        let router = get_or_create_router(&ctx, &session_id, &raw);

        // Acquire the handoff semaphore (blocks if another session
        // is mid-handoff — holds for milliseconds at most).
        let permit = ctx
            .handoff_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("handoff semaphore closed"))?;

        // Drain accumulated files from EditingManager.
        let (files, filtered) = router.session.editing.drain_all_and_clear();

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            file_count = files.len(),
            filtered,
            "editing stop: drained files from EditingManager",
        );

        // Release the editing guardrail.
        ctx.editing_guardrail.release_all(&session_id);

        // Mint the scope UUID for the done-editing IPC execution.
        // This is separate from the prepare handler's own scope_id —
        // the prepare hook is one scope, the IPC execution is another.
        let handoff_parent_id = uuid::Uuid::new_v4().to_string();

        // Deposit in the handoff slot.
        {
            let mut slot = ctx
                .handoff_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(HandoffContext {
                files,
                filtered,
                parent_id: handoff_parent_id,
                permit,
            });
        }

        // Spawn timeout to clear the slot if the CLI never connects.
        spawn_handoff_timeout(ctx.handoff_slot.clone());

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            "editing stop handoff prepared",
        );

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "{\"status\":\"ok\"}",
            "outgoing hook response",
        );

        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Done editing handoff: run ────────────────────────────────
    //
    // `tool/editing-stop` is sent by `catenary editing stop` CLI
    // command. Takes the file list from the handoff slot, runs
    // process_files_batched, and returns diagnostics.
    if method == "tool/editing-stop" {
        // Take the file list and parent_id from the handoff slot,
        // releasing the permit immediately. The permit must not be
        // held during the diagnostics pipeline (which may take seconds).
        let handoff = {
            let mut slot = ctx
                .handoff_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Destructure HandoffContext — dropping it releases the
            // owned semaphore permit.
            slot.take().map(|h| (h.files, h.filtered, h.parent_id))
        };

        // Extract scope_id early so we can emit the incoming hook
        // event before running the diagnostics pipeline. This ensures
        // the tool/editing-stop event is the first message in the
        // parent_id group, making it the scope header in the TUI
        // (matching the grep/glob pattern).
        let scope_id = match &handoff {
            Some((_, _, parent_id)) => parent_id.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );

        let response = if let Some((files, filtered, _)) = handoff {
            if files.is_empty() {
                if filtered > 0 {
                    "(edits outside tracked roots \u{2014} see `catenary roots -h`)\n".to_string()
                } else {
                    String::new()
                }
            } else {
                ctx.primary
                    .diagnostics
                    .process_files_batched(&files, Some(&scope_id))
                    .await
            }
        } else {
            // Handoff slot was empty — timeout expired or double-consume.
            "editing stop handoff expired — no files available\n".to_string()
        };

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&scope_id),
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
    // `tool/roots-add` and `tool/roots-rm` are sent by the CLI commands
    // (`catenary roots add`, `catenary roots rm`). The PreToolUse hook
    // only bypasses the command filter — no hook-side IPC needed
    // since "hook" is a shared contributor with no session identity.
    //
    // Handled before `get_or_create_router` because root management
    // is a daemon-level concern (RootTracker), not a per-session
    // router concern.
    if method == "tool/roots-add" {
        let scope_id = uuid::Uuid::new_v4().to_string();
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
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &response.to_string(),
            "outgoing hook response",
        );

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    if method == "tool/roots-rm" {
        let scope_id = uuid::Uuid::new_v4().to_string();
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
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &response.to_string(),
            "outgoing hook response",
        );

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    let router = get_or_create_router(&ctx, &session_id, &raw);

    // Span with session_id so warn!/error! events emitted during
    // hook dispatch route to the correct notification queue.
    let hook_span = tracing::info_span!(
        "hook_dispatch",
        session_id = %session_id,
    );
    let _hook_guard = hook_span.enter();

    // Mint a UUID for this request/response pair.
    let scope_id = uuid::Uuid::new_v4().to_string();

    let request: HookRequest =
        serde_json::from_value(raw.clone()).map_err(|e| anyhow!("Invalid hook request: {e}"))?;

    let result = router.dispatch(request);

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
        Some(&scope_id),
        &raw.to_string(),
        "incoming hook",
    );

    // Log outgoing hook response — same parent_id as request.
    emit_hook_event(
        level,
        &session_id,
        &method,
        Some(&scope_id),
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
        let _ = std::fs::remove_file(&self.ipc_socket_path);
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
    let mcp_path = mcp_socket_path();
    let mut daemon_spawned = false;

    for attempt in 0..MAX_CONNECT_ATTEMPTS {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(&mcp_path) {
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
                mcp_path.display(),
            );
        }

        if !daemon_spawned {
            if mcp_path.exists() {
                let _ = std::fs::remove_file(&mcp_path);
            }
            let ipc_path = socket_path();
            if ipc_path.exists() {
                let _ = std::fs::remove_file(&ipc_path);
            }
            spawn_daemon()?;
            daemon_spawned = true;
        }

        std::thread::sleep(CONNECT_RETRY_DELAY);
    }

    anyhow::bail!(
        "failed to connect to Catenary daemon ({})",
        mcp_path.display(),
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

    /// Create an MCP socket path inside a tempdir.
    fn mcp_socket_in(dir: &Path) -> PathBuf {
        dir.join("catenary").join("catenary-mcp.sock")
    }

    /// Create an IPC socket path inside a tempdir.
    fn ipc_socket_in(dir: &Path) -> PathBuf {
        dir.join("catenary").join("catenary.sock")
    }

    /// Bind a `SessionManager` with both sockets in a tempdir.
    fn bind_in(dir: &Path) -> SessionManager {
        SessionManager::bind_at(
            &mcp_socket_in(dir),
            &ipc_socket_in(dir),
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
        let ipc_path = ipc_socket_in(dir.path());

        let manager = bind_in(dir.path());
        assert!(mcp_path.exists(), "MCP socket should exist before drop");
        assert!(ipc_path.exists(), "IPC socket should exist before drop");

        drop(manager);

        assert!(
            !mcp_path.exists(),
            "MCP socket should be removed after drop"
        );
        assert!(
            !ipc_path.exists(),
            "IPC socket should be removed after drop"
        );
    }

    #[tokio::test]
    async fn bind_fails_if_mcp_socket_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        // Create a regular file at the MCP socket path.
        std::fs::create_dir_all(mcp_path.parent().expect("parent")).expect("create dir");
        std::fs::write(&mcp_path, b"").expect("create file");

        let result = SessionManager::bind_at(&mcp_path, &ipc_path, LoggingServer::new());
        assert!(
            result.is_err(),
            "bind should fail when MCP socket already exists"
        );
    }

    #[tokio::test]
    async fn bind_fails_if_ipc_socket_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        // Create a regular file at the IPC socket path.
        std::fs::create_dir_all(ipc_path.parent().expect("parent")).expect("create dir");
        std::fs::write(&ipc_path, b"").expect("create file");

        let result = SessionManager::bind_at(&mcp_path, &ipc_path, LoggingServer::new());
        assert!(
            result.is_err(),
            "bind should fail when IPC socket already exists"
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
        let ipc_path = ipc_socket_in(dir.path());

        let _manager = tracing::subscriber::with_default(subscriber, || {
            SessionManager::bind_at(&mcp_path, &ipc_path, LoggingServer::new())
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

    // ── IPC socket tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn ipc_socket_created_on_bind() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let _manager = bind_in(dir.path());

        assert!(ipc_path.exists(), "IPC socket file should exist after bind");
    }

    #[tokio::test]
    async fn ipc_connection_accepted() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _stream = tokio::net::UnixStream::connect(&ipc_path)
            .await
            .expect("connect to IPC socket");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn ipc_and_mcp_sockets_independent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Connect to both sockets simultaneously.
        let (mcp_result, ipc_result) = tokio::join!(
            tokio::net::UnixStream::connect(&mcp_path),
            tokio::net::UnixStream::connect(&ipc_path),
        );

        let mcp_stream = mcp_result.expect("connect to MCP socket");
        let _ipc_stream = ipc_result.expect("connect to IPC socket");

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
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = tokio::net::UnixStream::connect(&ipc_path)
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
            let mut mcp = McpServer::new(logging);
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

        let manager = Arc::new(bind_in(dir.path()));
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
    async fn per_connection_tools_list_returns_method_not_found() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
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

        // tools/list should return method-not-found (no tools on MCP).
        let response = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }),
        );

        assert!(
            response.get("error").is_some(),
            "tools/list should return error, got: {response}",
        );

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_cleanup_on_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
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
        let ipc_path = ipc_socket_in(dir.path());

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
            !ipc_path.exists(),
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
    async fn stop_via_ipc_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Send shutdown via IPC socket.
        let stream = tokio::net::UnixStream::connect(&ipc_path)
            .await
            .expect("connect to IPC socket");
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
            !ipc_path.exists(),
            "IPC socket should be removed after stop",
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

        SessionManager::bind_at(&mcp_socket_in(dir), &ipc_socket_in(dir), logging)
            .expect("bind")
            .with_session(session, conn)
    }

    /// Send a hook JSON request and read the response line.
    async fn hook_roundtrip(ipc_path: &Path, request: &serde_json::Value) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stream = tokio::net::UnixStream::connect(ipc_path)
            .await
            .expect("connect to IPC socket");
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
        let ipc_path = ipc_socket_in(dir.path());

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
        let _ = hook_roundtrip(&ipc_path, &request).await;

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
        let ipc_path = ipc_socket_in(dir.path());

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
        let _ = hook_roundtrip(&ipc_path, &req_a).await;
        let _ = hook_roundtrip(&ipc_path, &req_b).await;

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
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Session A: enter editing mode via CLI start_editing hook.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "session-a"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Session A: Edit should be allowed (in editing mode).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "agent_id": "",
            "session_id": "session-a"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
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
        let line = hook_roundtrip(&ipc_path, &req).await;
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
        let ipc_path = ipc_socket_in(dir.path());

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
        let _ = hook_roundtrip(&ipc_path, &req_a).await;
        let _ = hook_roundtrip(&ipc_path, &req_a).await;

        // Send one turn-start hook to session B.
        let req_b = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "session-b"
        });
        let _ = hook_roundtrip(&ipc_path, &req_b).await;

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
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Hook with non-empty agent_id should pass through without
        // triggering editing enforcement.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Read",
            "agent_id": "sub-agent-1",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
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
    async fn hook_roundtrip_full(ipc_path: &Path, request: &serde_json::Value) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::UnixStream::connect(ipc_path)
            .await
            .expect("connect to IPC socket");

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
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Prepare handoff (no files accumulated).
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — no edits at all, silent output.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            response.trim().is_empty(),
            "expected empty output for no edits, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_out_of_roots() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Edit a file outside workspace roots — filtered, not accumulated.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": "/outside/some/file.rs",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Prepare handoff — files empty but filtered > 0.
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — should get out-of-roots message.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            response.contains("edits outside tracked roots"),
            "expected out-of-roots message, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_expired() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Call done-editing/run without preparing a handoff.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            response.contains("handoff expired"),
            "expected handoff expired message, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_with_accumulated_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Accumulate a file via pre-tool hook (file tracking).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": "/tmp/nonexistent_file.rs",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Prepare handoff — should drain the accumulated file.
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — diagnostics pipeline runs on
        // the files. Since there's no real LSP server, the output
        // depends on whether the file exists and has LSP coverage.
        // The key test: the handoff consumed the files successfully.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
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
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode and prepare handoff.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // First consume should succeed.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response1 = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            !response1.contains("handoff expired"),
            "first consume should succeed, got: {response1}",
        );

        // Second consume should see expired slot.
        let response2 = hook_roundtrip_full(&ipc_path, &req).await;
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

    #[test]
    fn list_roots_returns_sorted_with_sources() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/b"), PathBuf::from("/a")]);
        tracker.add_roots("hook", &[PathBuf::from("/a")]);

        let listed = tracker.list_roots();
        assert_eq!(listed.len(), 2);
        // Sorted by path.
        assert_eq!(listed[0].0, PathBuf::from("/a"));
        assert_eq!(listed[0].1, vec!["hook", "mcp:10"]);
        assert_eq!(listed[1].0, PathBuf::from("/b"));
        assert_eq!(listed[1].1, vec!["mcp:10"]);
    }

    #[test]
    fn list_roots_empty_tracker() {
        let tracker = RootTracker::new();
        assert!(tracker.list_roots().is_empty());
    }

    // ── Function-level tests (mutant audit 03-07) ─────────────────

    /// `mcp_socket_path` returns a deterministic path inside `state_dir`.
    #[test]
    fn test_mcp_socket_path_structure() {
        let path = mcp_socket_path();
        assert!(
            path.ends_with("catenary/catenary-mcp.sock"),
            "mcp_socket_path should end with catenary/catenary-mcp.sock, got: {}",
            path.display()
        );
    }

    /// `socket_path` returns a deterministic path inside `state_dir`.
    #[test]
    fn test_socket_path_structure() {
        let path = socket_path();
        assert!(
            path.ends_with("catenary/catenary.sock"),
            "socket_path should end with catenary/catenary.sock, got: {}",
            path.display()
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

    // ── IPC request/response type tests ──────────────────────────

    /// `GrepRequest` roundtrips through JSON with all fields.
    #[test]
    fn grep_request_roundtrip_full() {
        let req = GrepRequest {
            cwd: Some(PathBuf::from("/home/user/project")),
            pattern: "TODO|FIXME".to_string(),
            paths: vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")],
            exclude: Some("tests/**".to_string()),
            page: 2,
            include_gitignored: true,
            include_hidden: false,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: GrepRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/home/user/project")));
        assert_eq!(parsed.pattern, "TODO|FIXME");
        assert_eq!(
            parsed.paths,
            vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")]
        );
        assert_eq!(parsed.exclude.as_deref(), Some("tests/**"));
        assert_eq!(parsed.page, 2);
        assert!(parsed.include_gitignored);
        assert!(!parsed.include_hidden);
    }

    /// `GrepRequest` deserializes with defaults for optional fields.
    #[test]
    fn grep_request_minimal() {
        let json = r#"{"cwd":"/tmp","pattern":"foo"}"#;
        let req: GrepRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(req.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(req.pattern, "foo");
        assert!(req.exclude.is_none());
        assert_eq!(req.page, 1);
        assert!(!req.include_gitignored);
        assert!(!req.include_hidden);
    }

    /// `GrepRequest` skips empty/`None` fields in serialized output.
    #[test]
    fn grep_request_skips_none_fields() {
        let req = GrepRequest {
            cwd: Some(PathBuf::from("/tmp")),
            pattern: "foo".to_string(),
            paths: vec![],
            exclude: None,
            page: 1,
            include_gitignored: false,
            include_hidden: false,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(!json.contains("paths"), "empty paths should be skipped");
        assert!(!json.contains("exclude"), "None exclude should be skipped");
    }

    /// `GrepResponse` roundtrips through JSON.
    #[test]
    fn grep_response_roundtrip() {
        let resp = GrepResponse {
            output: "file.rs:10 matched line".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: GrepResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.output, "file.rs:10 matched line");
    }

    /// `GlobRequest` roundtrips through JSON with all fields.
    #[test]
    fn glob_request_roundtrip_full() {
        let req = GlobRequest {
            cwd: Some(PathBuf::from("/workspace")),
            paths: vec![PathBuf::from("src/"), PathBuf::from("tests/")],
            exclude: Some("target/**".to_string()),
            page: 3,
            include_gitignored: false,
            include_hidden: true,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: GlobRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/workspace")));
        assert_eq!(
            parsed.paths,
            vec![PathBuf::from("src/"), PathBuf::from("tests/")]
        );
        assert_eq!(parsed.exclude.as_deref(), Some("target/**"));
        assert_eq!(parsed.page, 3);
        assert!(!parsed.include_gitignored);
        assert!(parsed.include_hidden);
    }

    /// `GlobRequest` deserializes with defaults for optional fields.
    #[test]
    fn glob_request_minimal() {
        let json = r#"{"cwd":"/home","paths":["src/"]}"#;
        let req: GlobRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(req.cwd, Some(PathBuf::from("/home")));
        assert_eq!(req.paths, vec![PathBuf::from("src/")]);
        assert!(req.exclude.is_none());
        assert_eq!(req.page, 1);
        assert!(!req.include_gitignored);
        assert!(!req.include_hidden);
    }

    /// `GlobResponse` roundtrips through JSON.
    #[test]
    fn glob_response_roundtrip() {
        let resp = GlobResponse {
            output: "src/\n  main.rs (42 lines)".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: GlobResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.output, "src/\n  main.rs (42 lines)");
    }

    /// IPC method constants match expected wire values.
    #[test]
    fn method_constants() {
        assert_eq!(METHOD_GREP, "tool/grep");
        assert_eq!(METHOD_GLOB, "tool/glob");
    }

    // ── resolve_relative tests ──────────────────────────────────

    #[test]
    fn resolve_relative_absolute_unchanged() {
        let result = resolve_relative("/tmp/src/**/*.rs", Path::new("/home/user"));
        assert_eq!(result, "/tmp/src/**/*.rs");
    }

    #[test]
    fn resolve_relative_relative_joined() {
        let result = resolve_relative("src/**/*.rs", Path::new("/home/user/project"));
        assert_eq!(result, "/home/user/project/src/**/*.rs");
    }

    #[test]
    fn resolve_relative_tilde_expanded() {
        let result = resolve_relative("~/src/**/*.rs", Path::new("/home/user/project"));
        // Tilde-expanded paths are absolute → not joined to base.
        assert!(
            !result.starts_with("/home/user/project"),
            "tilde path should not be joined to base"
        );
    }
}
