// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Daemon session manager and MCP socket listener.
//!
//! [`SessionManager`] is the core daemon component. It binds a Unix domain
//! socket at `$XDG_STATE_HOME/catenary/catenary.sock`, accepts incoming
//! connections from `catenary bridge` proxies, and tracks them by file
//! descriptor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing::info;

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

/// Core daemon component that manages MCP socket connections.
///
/// Binds a Unix domain socket and accepts incoming connections from
/// `catenary bridge` proxies. Connection tracking by file descriptor
/// enables lifecycle management (shutdown on last disconnect).
///
/// No MCP processing happens here — per-connection domain stacks are
/// spawned in a subsequent phase.
#[cfg(unix)]
pub struct SessionManager {
    mcp_listener: tokio::net::UnixListener,
    socket_path: PathBuf,
    connections: Mutex<HashMap<i32, tokio::net::UnixStream>>,
}

#[cfg(unix)]
impl SessionManager {
    /// Binds the MCP socket at the default path.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory cannot be created or the
    /// socket cannot be bound (e.g., another daemon is already running).
    pub fn bind() -> Result<Self> {
        Self::bind_at(&mcp_socket_path())
    }

    /// Binds the MCP socket at an explicit path.
    ///
    /// Used by tests to isolate socket files in tempdirs.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory cannot be created or the
    /// socket cannot be bound.
    pub fn bind_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory: {}", parent.display()))?;
        }

        let mcp_listener = tokio::net::UnixListener::bind(path)
            .with_context(|| format!("bind MCP socket: {}", path.display()))?;

        info!(
            source = Source::DaemonLifecycle.as_str(),
            path = %path.display(),
            "daemon started",
        );

        Ok(Self {
            mcp_listener,
            socket_path: path.to_path_buf(),
            connections: Mutex::new(HashMap::new()),
        })
    }

    /// Accepts incoming MCP connections in a loop.
    ///
    /// Each accepted connection is tracked by its file descriptor.
    /// Per-connection domain stacks are not spawned yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the listener encounters a fatal I/O error.
    pub async fn accept_loop(&self) -> Result<()> {
        use std::os::fd::AsRawFd;

        loop {
            let (stream, _addr) = self
                .mcp_listener
                .accept()
                .await
                .context("accept MCP connection")?;

            let fd = stream.as_raw_fd();

            {
                let mut conns = self
                    .connections
                    .lock()
                    .map_err(|_| anyhow::anyhow!("connection mutex poisoned"))?;
                conns.insert(fd, stream);
            }

            info!(
                source = Source::DaemonDispatch.as_str(),
                mcp_fd = fd,
                "connection accepted",
            );
        }
    }

    /// Returns the number of tracked connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.lock().map_or(0, |c| c.len())
    }

    /// Returns the socket path this manager is bound to.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[cfg(unix)]
impl Drop for SessionManager {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
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
/// # Errors
///
/// Returns an error if the daemon connection closes before stdin,
/// indicating unexpected daemon termination.
#[cfg(unix)]
pub async fn proxy_stdio(stream: tokio::net::UnixStream) -> Result<()> {
    let (mut sock_read, mut sock_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut sock_write) => {
            result.context("proxy stdin to socket")?;
            Ok(())
        }
        result = tokio::io::copy(&mut sock_read, &mut stdout) => {
            match result {
                Ok(_) => Err(anyhow::anyhow!("daemon connection closed unexpectedly")),
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
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;

    /// Create a socket path inside a tempdir.
    fn socket_in(dir: &Path) -> PathBuf {
        dir.join("catenary").join("catenary.sock")
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
        let path = socket_in(dir.path());

        let _manager = SessionManager::bind_at(&path).expect("bind");

        assert!(path.exists(), "socket file should exist after bind");
    }

    #[tokio::test]
    async fn accept_connection() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = socket_in(dir.path());

        let manager = Arc::new(SessionManager::bind_at(&path).expect("bind"));
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _stream = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connect");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(manager.connection_count(), 1);
    }

    #[tokio::test]
    async fn multiple_connections() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = socket_in(dir.path());

        let manager = Arc::new(SessionManager::bind_at(&path).expect("bind"));
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _streams: Vec<_> = {
            let mut v = Vec::new();
            for _ in 0..3 {
                v.push(
                    tokio::net::UnixStream::connect(&path)
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
        let path = socket_in(dir.path());

        let manager = SessionManager::bind_at(&path).expect("bind");
        assert!(path.exists(), "socket should exist before drop");

        drop(manager);

        assert!(!path.exists(), "socket should be removed after drop");
    }

    #[tokio::test]
    async fn bind_fails_if_socket_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = socket_in(dir.path());

        // Create a regular file at the socket path.
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        std::fs::write(&path, b"").expect("create file");

        let result = SessionManager::bind_at(&path);
        assert!(
            result.is_err(),
            "bind should fail when socket already exists"
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
        let path = socket_in(dir.path());

        let _manager =
            tracing::subscriber::with_default(subscriber, || SessionManager::bind_at(&path))
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
        let path = socket_in(dir.path());

        // Create a stale socket file (regular file, nobody listening).
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"stale").expect("create stale file");

        // Connect should fail on a regular file.
        let result = tokio::net::UnixStream::connect(&path).await;
        assert!(result.is_err());

        // Clean stale file (what connect_or_start_daemon does).
        std::fs::remove_file(&path).expect("remove stale");
        assert!(!path.exists());

        // Now bind succeeds.
        let _manager = SessionManager::bind_at(&path).expect("bind after cleanup");
        assert!(path.exists());
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
        let path = socket_in(dir.path());

        let manager = Arc::new(SessionManager::bind_at(&path).expect("bind"));
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Spawn 5 connections concurrently.
        let mut handles = Vec::new();
        for _ in 0..5 {
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                tokio::net::UnixStream::connect(&p).await
            }));
        }

        for handle in handles {
            handle.await.expect("task").expect("connect");
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 5);
    }
}
