// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Daemon session manager and socket listeners.
//!
//! [`SessionManager`] is the core daemon component. It binds two Unix domain
//! sockets — one for MCP connections from `catenary bridge` proxies, one for
//! hook connections from `catenary hook` CLI processes — and tracks MCP
//! connections by file descriptor. Hook connections are short-lived
//! (one request-response each).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow};
use tracing::{Instrument, debug, error, info};

use crate::bridge::HookRouter;
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

/// Shared context for session-aware hook dispatch.
///
/// When set on [`SessionManager`], hook connections are routed to
/// per-`session_id` [`HookRouter`] instances backed by the shared
/// daemon [`Session`]. When absent, hooks receive passthrough responses
/// (allow everything).
#[cfg(unix)]
#[derive(Clone)]
struct HookDispatchContext {
    /// Per-`session_id` hook routers. Each router has its own turn
    /// counter and debounce state; all share the daemon's `Session`.
    sessions: Arc<std::sync::Mutex<HashMap<String, Arc<HookRouter>>>>,
    /// Shared daemon session (owns `LspClientManager`, config, editing
    /// state, etc.).
    session: Arc<Session>,
    /// Shared database connection for `HookRouter` DB writes.
    conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    /// Logging server for correlation ID minting and sink access.
    logging: LoggingServer,
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
                    if let Some(ctx) = &self.hook_ctx {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_hook_dispatch(stream, ctx).await {
                                debug!(
                                    source = Source::DaemonDispatch.as_str(),
                                    "hook connection error: {e}",
                                );
                            }
                        });
                    } else {
                        tokio::spawn(async move {
                            if let Err(e) = handle_hook_passthrough(stream).await {
                                debug!(
                                    source = Source::DaemonDispatch.as_str(),
                                    "hook connection error: {e}",
                                );
                            }
                        });
                    }
                }
            }
        }
    }

    /// Spawns a per-connection MCP task.
    ///
    /// Converts the tokio `UnixStream` to a `std::os::unix::net::UnixStream`
    /// (since `McpServer` uses synchronous I/O), clones it for
    /// read/write halves, and runs the MCP message loop in a blocking task.
    fn handle_mcp_connection(&self, stream: tokio::net::UnixStream, fd: i32) {
        let handler = self.handler.clone();
        let logging = self.logging.clone();
        let count = Arc::clone(&self.connection_count);

        count.fetch_add(1, Ordering::Relaxed);

        tokio::spawn(async move {
            let span = tracing::info_span!("mcp_connection", mcp_fd = fd);
            async {
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
                            count.fetch_sub(1, Ordering::Relaxed);
                            return;
                        }
                        s
                    }
                    Err(e) => {
                        error!(
                            source = Source::DaemonDispatch.as_str(),
                            "failed to convert socket to std: {e}",
                        );
                        count.fetch_sub(1, Ordering::Relaxed);
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
                        count.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                };
                let writer = std_stream;

                let result = tokio::task::spawn_blocking(move || {
                    let mut mcp = McpServer::new(handler, logging);
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

                count.fetch_sub(1, Ordering::Relaxed);
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
    /// Once set, hook connections are routed to per-`session_id`
    /// [`HookRouter`] instances backed by the shared daemon `Session`.
    /// Without this, hooks receive passthrough responses (test mode).
    #[must_use]
    pub fn with_session(
        mut self,
        session: Arc<Session>,
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    ) -> Self {
        self.hook_ctx = Some(HookDispatchContext {
            sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            session,
            conn,
            logging: self.logging.clone(),
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

/// Handles a single hook connection with a passthrough response.
///
/// Reads the JSON request, logs the method for visibility, and sends an
/// empty response (which means "allow" in the hook protocol). Used when
/// no shared session is configured (test mode).
#[cfg(unix)]
async fn handle_hook_passthrough(stream: tokio::net::UnixStream) -> Result<()> {
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

/// Looks up or creates a [`HookRouter`] for the given `session_id`.
///
/// Each `session_id` gets its own router with independent turn counter
/// and debounce state. All routers share the daemon's [`Session`]
/// (editing state is already keyed by `session_id` inside
/// [`crate::bridge::EditingManager`]).
#[cfg(unix)]
fn get_or_create_router(ctx: &HookDispatchContext, session_id: &str) -> Arc<HookRouter> {
    let mut sessions = ctx
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sessions
        .entry(session_id.to_string())
        .or_insert_with(|| {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id, "creating session",
            );
            Arc::new(HookRouter::new(
                ctx.session.clone(),
                ctx.conn.clone(),
                ctx.session.instance_id.clone(),
                session_id.to_string(),
            ))
        })
        .clone()
}

/// Handles a single hook connection with session-aware dispatch.
///
/// Reads the JSON request, extracts `session_id` for routing, looks up
/// (or creates) the per-session [`HookRouter`], dispatches the request,
/// handles transcript root syncing, logs the protocol pair, and writes
/// the response.
#[cfg(unix)]
#[allow(clippy::too_many_lines, reason = "sequential protocol steps")]
async fn handle_hook_dispatch(
    stream: tokio::net::UnixStream,
    ctx: HookDispatchContext,
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

    // Extract session_id for routing. Falls back to "default" for hooks
    // that don't carry a session_id (backward compatibility).
    let session_id = raw
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    let router = get_or_create_router(&ctx, &session_id);

    // Mint a correlation ID for this request/response pair.
    let id = ctx.logging.next_id();

    let request: HookRequest =
        serde_json::from_value(raw.clone()).map_err(|e| anyhow!("Invalid hook request: {e}"))?;

    let result = router.dispatch(request, id.0);

    // Apply transcript-discovered roots before responding.
    if !result.add_roots.is_empty() {
        let session = &router.session;
        let mut current = session.roots();
        let before = current.len();
        for root in &result.add_roots {
            if !current.contains(root) {
                current.push(root.clone());
            }
        }
        let added = current.len() - before;
        if added > 0 {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                added, "transcript root sync: syncing new roots",
            );
            if let Err(e) = session.sync_roots(current).await {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    "transcript root sync failed: {e}",
                );
            }
        }
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
    emit_hook_event(
        level,
        &session_id,
        &method,
        id.0,
        Some(id.0),
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
const MAX_CONNECT_ATTEMPTS: u32 = 5;

/// Delay between connection retry attempts.
const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

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
pub async fn connect_or_start_daemon() -> Result<tokio::net::UnixStream> {
    let socket_path = mcp_socket_path();
    let mut daemon_spawned = false;

    for attempt in 0..MAX_CONNECT_ATTEMPTS {
        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    attempt, "connected to daemon",
                );
                return Ok(stream);
            }
            Err(e) => {
                let last_attempt = attempt == MAX_CONNECT_ATTEMPTS - 1;
                if last_attempt {
                    return Err(e).with_context(|| {
                        format!(
                            "failed to connect to Catenary daemon \
                             after {MAX_CONNECT_ATTEMPTS} attempts ({})",
                            socket_path.display(),
                        )
                    });
                }

                if !daemon_spawned {
                    if socket_path.exists() {
                        let _ = std::fs::remove_file(&socket_path);
                        info!(
                            source = Source::DaemonLifecycle.as_str(),
                            path = %socket_path.display(),
                            "removed stale socket",
                        );
                    }
                    let hook_path = hook_socket_path();
                    if hook_path.exists() {
                        let _ = std::fs::remove_file(&hook_path);
                        info!(
                            source = Source::DaemonLifecycle.as_str(),
                            path = %hook_path.display(),
                            "removed stale hook socket",
                        );
                    }
                    if spawn_daemon().is_ok() {
                        info!(
                            source = Source::DaemonLifecycle.as_str(),
                            "spawned daemon process",
                        );
                    }
                    daemon_spawned = true;
                }

                tokio::time::sleep(CONNECT_RETRY_DELAY).await;
            }
        }
    }

    // Loop always returns: Ok on connect, Err on last attempt.
    anyhow::bail!(
        "failed to connect to Catenary daemon ({})",
        socket_path.display(),
    )
}

/// Spawns `catenary daemon` as a detached child process.
///
/// The daemon binds the MCP socket and begins accepting connections.
/// Uses a new process group so the daemon outlives the bridge.
#[cfg(unix)]
fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("resolve current executable path")?;

    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawn daemon process")?;

    Ok(())
}

/// Proxies stdin/stdout to/from a daemon socket connection.
///
/// Runs two concurrent copy loops. Returns `Ok(())` when stdin closes
/// (host CLI ended the session) or `Err` when the daemon connection
/// drops.
///
/// The stdin→socket direction uses a dedicated OS thread with blocking
/// I/O because `tokio::io::Stdin` internally blocks a threadpool thread
/// that cannot be cancelled ([tokio#2466]). A blocking `std::io::copy`
/// on a dedicated thread avoids this and exits cleanly on EOF.
///
/// The socket→stdout direction uses a read-write-flush loop because
/// `std::io::Stdout` uses full buffering on pipes. Without explicit
/// flushing, MCP responses sit in the buffer until it fills (8 KB).
///
/// [tokio#2466]: https://github.com/tokio-rs/tokio/issues/2466
///
/// # Errors
///
/// Returns an error if the daemon connection closes before stdin,
/// indicating unexpected daemon termination.
#[cfg(unix)]
pub async fn proxy_stdio(stream: tokio::net::UnixStream) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Convert to std for the blocking stdin→socket direction.
    // try_clone gives us a second handle to the same fd so we can
    // split read/write without consuming the tokio stream.
    let std_stream = stream.into_std().context("convert daemon socket to std")?;
    let std_write = std_stream
        .try_clone()
        .context("clone daemon socket for writer")?;

    // Wrap the read half back into tokio for async socket→stdout.
    std_stream
        .set_nonblocking(true)
        .context("set socket non-blocking")?;
    let mut sock_read = tokio::net::UnixStream::from_std(std_stream)
        .context("re-wrap socket read half as tokio")?;

    // stdin→socket: dedicated OS thread with blocking I/O.
    // std_write is already in blocking mode (into_std default).
    std_write
        .set_nonblocking(false)
        .context("set write half blocking")?;
    let stdin_task = tokio::task::spawn_blocking(move || {
        let mut stdin = std::io::stdin().lock();
        let mut writer = std_write;
        std::io::copy(&mut stdin, &mut writer).context("proxy stdin to socket")
    });

    // socket→stdout: async read with explicit flush after each chunk.
    let mut stdout = tokio::io::stdout();

    tokio::select! {
        result = stdin_task => {
            result.context("stdin proxy task")??;
            Ok(())
        }
        result = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = sock_read.read(&mut buf).await?;
                if n == 0 {
                    return Ok::<(), std::io::Error>(());
                }
                stdout.write_all(&buf[..n]).await?;
                stdout.flush().await?;
            }
        } => {
            match result {
                Ok(()) => Err(anyhow::anyhow!("daemon connection closed unexpectedly")),
                Err(e) => Err(anyhow::Error::from(e).context("daemon connection error")),
            }
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
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
            _parent_id: Option<i64>,
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
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _stream = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(manager.connection_count(), 1);
    }

    #[tokio::test]
    async fn multiple_connections() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _streams: Vec<_> = {
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

        #[allow(clippy::collection_is_never_read, reason = "held for Drop")]
        let mut streams = Vec::new();
        for handle in handles {
            streams.push(handle.await.expect("task").expect("connect"));
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 5);
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
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _stream = tokio::net::UnixStream::connect(&hook_path)
            .await
            .expect("connect to hook socket");
    }

    #[tokio::test]
    async fn hook_and_mcp_sockets_independent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Connect to both sockets simultaneously.
        let (mcp_result, hook_result) = tokio::join!(
            tokio::net::UnixStream::connect(&mcp_path),
            tokio::net::UnixStream::connect(&hook_path),
        );

        let _mcp_stream = mcp_result.expect("connect to MCP socket");
        let _hook_stream = hook_result.expect("connect to hook socket");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Only MCP connections are tracked.
        assert_eq!(manager.connection_count(), 1);
    }

    #[tokio::test]
    async fn hook_passthrough_response() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
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
            _parent_id: Option<i64>,
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_connection_tools_list() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo(dir.path()));
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_cleanup_on_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_echo(dir.path()));
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
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default(),
            vec![],
            logging.clone(),
            conn.clone(),
            instance_id,
            runtime,
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_hook_routes_to_correct_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_editing_per_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Session A: enter editing mode via start_editing pre-tool hook.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_start_editing",
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_turn_counter_per_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
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
        let router_a = Arc::clone(sessions.get("session-a").expect("session-a"));
        let router_b = Arc::clone(sessions.get("session-b").expect("session-b"));
        drop(sessions);
        assert_eq!(router_a.turn(), 2, "session A should have turn 2");
        assert_eq!(router_b.turn(), 1, "session B should have turn 1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_subagent_passthrough() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let hook_path = hook_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
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
    }
}
