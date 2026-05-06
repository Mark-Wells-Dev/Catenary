// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration tests for daemon transport (ticket 02).
//!
//! Tests spawn the `catenary daemon` subcommand as a subprocess and
//! verify socket creation, connection acceptance, and byte flow.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Wrapper around a `catenary daemon` subprocess with cleanup on drop.
struct DaemonProcess {
    child: std::process::Child,
    state_dir: tempfile::TempDir,
}

impl DaemonProcess {
    /// Spawns `catenary daemon` in an isolated tempdir.
    fn spawn() -> Result<Self> {
        let state_dir = tempfile::tempdir().context("create tempdir")?;
        let state_home = state_dir
            .path()
            .to_str()
            .context("state dir to str")?
            .to_string();

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        common::isolate_env(&mut cmd, &state_home);
        cmd.arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn().context("spawn daemon")?;

        Ok(Self { child, state_dir })
    }

    /// Returns the expected MCP socket path.
    fn socket_path(&self) -> PathBuf {
        self.state_dir.path().join("catenary").join("catenary.sock")
    }

    /// Blocks until the socket file appears on disk or timeout expires.
    fn wait_for_socket(&self, timeout: Duration) -> bool {
        let path = self.socket_path();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[test]
fn daemon_creates_socket_file() {
    let daemon = DaemonProcess::spawn().expect("spawn daemon");

    assert!(
        daemon.wait_for_socket(Duration::from_secs(5)),
        "daemon should create socket file within 5s",
    );
}

#[tokio::test]
async fn bridge_connects_to_daemon_process() {
    let daemon = DaemonProcess::spawn().expect("spawn daemon");
    let sock = daemon.socket_path();

    assert!(
        daemon.wait_for_socket(Duration::from_secs(5)),
        "daemon socket should appear",
    );

    let stream = tokio::net::UnixStream::connect(&sock)
        .await
        .expect("connect to daemon");
    assert!(stream.peer_addr().is_ok());
}

#[tokio::test]
async fn bridge_handles_race_with_daemon() {
    let daemon = DaemonProcess::spawn().expect("spawn daemon");
    let sock = daemon.socket_path();

    assert!(
        daemon.wait_for_socket(Duration::from_secs(5)),
        "daemon socket should appear",
    );

    // Connect 5 clients concurrently.
    let mut handles = Vec::new();
    for _ in 0..5 {
        let p = sock.clone();
        handles.push(tokio::spawn(async move {
            tokio::net::UnixStream::connect(&p).await
        }));
    }

    for handle in handles {
        handle.await.expect("task").expect("connect");
    }
}

#[tokio::test]
async fn bridge_proxies_bytes_through_daemon() {
    use tokio::io::AsyncWriteExt;

    let daemon = DaemonProcess::spawn().expect("spawn daemon");
    let sock = daemon.socket_path();

    assert!(
        daemon.wait_for_socket(Duration::from_secs(5)),
        "daemon socket should appear",
    );

    let stream = tokio::net::UnixStream::connect(&sock)
        .await
        .expect("connect");

    // Write bytes to the daemon socket — they are accepted by
    // SessionManager's accept_loop (which stores the stream). The
    // daemon doesn't echo, so we verify the write succeeds without
    // error (the connection is live and writable).
    let (_read_half, mut write_half) = stream.into_split();
    write_half.write_all(b"hello").await.expect("write");
    write_half.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn bridge_exits_on_daemon_death() {
    use tokio::io::AsyncReadExt;

    let mut daemon = DaemonProcess::spawn().expect("spawn daemon");
    let sock = daemon.socket_path();

    assert!(
        daemon.wait_for_socket(Duration::from_secs(5)),
        "daemon socket should appear",
    );

    let client = tokio::net::UnixStream::connect(&sock)
        .await
        .expect("connect");

    // Kill the daemon.
    daemon.child.kill().expect("kill daemon");
    daemon.child.wait().expect("wait daemon");

    // Client should see EOF or connection reset — both indicate
    // the daemon is gone.
    let (mut read_half, _write_half) = client.into_split();
    let mut buf = Vec::new();
    match read_half.read_to_end(&mut buf).await {
        Ok(n) => assert_eq!(n, 0, "expected EOF, got {n} bytes"),
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset,
            "expected ConnectionReset, got {e}",
        ),
    }
}
