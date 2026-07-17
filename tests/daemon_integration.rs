// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration tests for the daemon architecture.
//!
//! Transport tests (ticket 02) spawn `catenary daemon` directly and
//! verify socket creation, connection acceptance, and byte flow.
//!
//! Multi-session tests (ticket 13) spawn multiple `BridgeProcess`
//! instances sharing a single daemon via a shared `XDG_STATE_HOME`
//! and verify session isolation, stale socket recovery, stop command,
//! and cross-session editing guardrails.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(clippy::panic, reason = "tests use panic for diagnostics")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use common::{BridgeProcess, mockls_lsp_arg};

/// Wrapper around a `catenary daemon` subprocess with cleanup on drop.
struct DaemonProcess {
    child: std::process::Child,
    state_dir: tempfile::TempDir,
    _stderr_log: PathBuf,
}

impl DaemonProcess {
    /// Spawns `catenary daemon` in an isolated tempdir.
    ///
    /// Daemon stderr is captured to `daemon_stderr.log` inside the
    /// state dir for post-failure inspection.
    fn spawn() -> Result<Self> {
        let state_dir = tempfile::tempdir().context("create tempdir")?;
        let state_home = state_dir
            .path()
            .to_str()
            .context("state dir to str")?
            .to_string();

        let stderr_log = state_dir.path().join("daemon_stderr.log");
        let stderr_file = std::fs::File::create(&stderr_log).context("create daemon stderr log")?;

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        common::isolate_env(&mut cmd, &state_home);
        cmd.arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));

        let child = cmd.spawn().context("spawn daemon")?;

        Ok(Self {
            child,
            state_dir,
            _stderr_log: stderr_log,
        })
    }

    /// Returns the expected MCP socket path.
    fn socket_path(&self) -> PathBuf {
        common::xdg_state_home(self.state_dir.path())
            .join("catenary")
            .join("catenary-mcp.sock")
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

#[test]
fn daemon_starts_with_servers_configured() {
    let state_dir = tempfile::tempdir().expect("create tempdir");
    let state_home = state_dir
        .path()
        .to_str()
        .expect("state dir to str")
        .to_string();

    let root = tempfile::tempdir().expect("create root");
    let lsp = common::mockls_lsp_arg("mock_a", "");

    let stderr_log = state_dir.path().join("daemon_stderr.log");
    let stderr_file = std::fs::File::create(&stderr_log).expect("create stderr");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    common::isolate_env(&mut cmd, &state_home);
    cmd.arg("daemon")
        .env("CATENARY_SERVERS", &lsp)
        .env("CATENARY_ROOTS", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));

    let mut child = cmd.spawn().expect("spawn daemon");

    let sock = common::xdg_state_home(state_dir.path())
        .join("catenary")
        .join("catenary-mcp.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut appeared = false;
    while Instant::now() < deadline {
        if sock.exists() {
            appeared = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if !appeared {
        let status = child.try_wait().ok().flatten();
        let stderr_buf = std::fs::read_to_string(&stderr_log).unwrap_or_default();
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "daemon did not create socket within 5s. exit status: {status:?}, stderr:\n{stderr_buf}"
        );
    }

    let _ = child.kill();
    let _ = child.wait();
}

// ── Multi-session integration tests (ticket 13) ───────────────────

// The `mockls-event` persona (blessed, event discipline; diagnostics-debt 04c)
// so the mock is a diagnostics source by manifest membership — the scoped-receipt
// and cross-session-guardrail tests need real coverage. Its bundle is empty, so
// the extra `--scan-roots` some tests pass still drives behaviour. Doubles as the
// server key, language, and file extension.
const MOCK_LANG: &str = "mockls-event";

/// Returns the IPC socket path for a given state home.
fn ipc_socket_in(state_home: &str) -> PathBuf {
    common::xdg_state_home(state_home)
        .join("catenary")
        .join("catenary.sock")
}

/// Waits for the IPC socket to appear (up to 5 seconds).
fn wait_for_ipc_socket(state_home: &str) -> PathBuf {
    let path = ipc_socket_in(state_home);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "IPC socket not found at {} within 5s",
            path.display(),
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    path
}

/// Sends a hook JSON request and reads the response line.
fn hook_roundtrip(ipc_path: &Path, request: &serde_json::Value) -> Result<String> {
    use std::io::{Read, Write};

    let mut stream =
        std::os::unix::net::UnixStream::connect(ipc_path).context("connect to IPC socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{request}").context("write to IPC socket")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    Ok(response)
}

/// Two bridges sharing a daemon can both initialize and call grep.
#[test]
fn two_bridges_share_daemon() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;

    let file = root.path().join(format!("hello.{MOCK_LANG}"));
    std::fs::write(&file, "fn shared_sym()\nshared_sym\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");

    // Bridge A starts the daemon.
    let mut bridge_a = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge_a.initialize()?;

    // Bridge B connects to the existing daemon.
    let mut bridge_b = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge_b.initialize()?;

    // Both can call grep.
    let text_a = bridge_a.call_tool_text(
        "grep",
        &json!({"pattern": "shared_sym", "directory": root_str}),
    )?;
    assert!(
        text_a.contains("shared_sym"),
        "bridge A grep should find symbol, got:\n{text_a}",
    );

    let text_b = bridge_b.call_tool_text(
        "grep",
        &json!({"pattern": "shared_sym", "directory": root_str}),
    )?;
    assert!(
        text_b.contains("shared_sym"),
        "bridge B grep should find symbol, got:\n{text_b}",
    );

    Ok(())
}

/// Disconnecting one bridge preserves the other's session.
///
/// Uses `initialize_with_roots` so both bridges register their roots
/// via `roots/list`. Without this, the root tracker would be empty and
/// disconnecting one bridge would call `sync_roots([])`, shutting down
/// all LSP servers.
#[test]
fn bridge_disconnect_preserves_other() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;

    let file = root.path().join(format!("keep.{MOCK_LANG}"));
    std::fs::write(&file, "fn survivor()\nsurvivor\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");

    let mut bridge_a = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge_a.initialize_with_roots(&[root_str])?;

    let mut bridge_b = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge_b.initialize_with_roots(&[root_str])?;

    // Bridge A can grep.
    let text = bridge_a.call_tool_text(
        "grep",
        &json!({"pattern": "survivor", "directory": root_str}),
    )?;
    assert!(
        text.contains("survivor"),
        "bridge A should work before disconnect"
    );

    // Drop bridge A — daemon stays alive for bridge B. Root refcount
    // goes from 2 to 1, so the LSP server stays up.
    drop(bridge_a);
    std::thread::sleep(Duration::from_millis(500));

    // Bridge B still works.
    let text = bridge_b.call_tool_text(
        "grep",
        &json!({"pattern": "survivor", "directory": root_str}),
    )?;
    assert!(
        text.contains("survivor"),
        "bridge B should still work after A disconnects, got:\n{text}",
    );

    Ok(())
}

/// Bridge cleans up a stale socket file and starts a fresh daemon.
#[test]
fn stale_socket_recovery() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    let file = root.path().join(format!("stale.{MOCK_LANG}"));
    std::fs::write(&file, "fn recovered()\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");

    // Create a stale socket file (regular file, nobody listening).
    let socket_dir = common::xdg_state_home(state_dir.path()).join("catenary");
    std::fs::create_dir_all(&socket_dir)?;
    std::fs::write(socket_dir.join("catenary-mcp.sock"), b"stale")?;

    // Bridge should detect the stale socket, remove it, start daemon,
    // and connect successfully.
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;

    // Verify the bridge works.
    let text = bridge.call_tool_text(
        "grep",
        &json!({"pattern": "recovered", "directory": root_str}),
    )?;
    assert!(
        text.contains("recovered"),
        "bridge should work after stale socket recovery, got:\n{text}",
    );

    Ok(())
}

/// `catenary stop` shuts down the daemon and cleans up socket files.
#[test]
fn stop_command_shuts_down_daemon() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");

    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;

    // Verify sockets exist before stop.
    let sock_dir = common::xdg_state_home(state_dir.path()).join("catenary");
    let mcp_sock = sock_dir.join("catenary-mcp.sock");
    let ipc_sock = sock_dir.join("catenary.sock");
    assert!(mcp_sock.exists(), "MCP socket should exist before stop");

    // Run `catenary stop` targeting the same state dir.
    let mut stop_cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    common::isolate_env(&mut stop_cmd, state_home);
    stop_cmd.arg("stop");
    let output = stop_cmd.output().context("run catenary stop")?;
    assert!(
        output.status.success(),
        "catenary stop should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Wait for the daemon's accept_loop to exit and remove sockets.
    let deadline = Instant::now() + Duration::from_secs(5);
    while mcp_sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        !mcp_sock.exists(),
        "MCP socket should be removed after stop",
    );
    assert!(
        !ipc_sock.exists(),
        "IPC socket should be removed after stop",
    );

    // Drop bridge — the daemon connection is gone, so the bridge
    // process will exit naturally (or be killed by Drop).
    drop(bridge);

    Ok(())
}

/// Cross-session editing guardrail: session A's first edit to a root
/// blocks session B's first edit to the same root.
///
/// Each session needs its own bridge (MCP connection) and is correlated
/// first via a grep `tools/call`. With implicit editing start, the first
/// Edit both enters editing mode and acquires the per-root guardrail — no
/// explicit `editing start` is needed, so session A claims the root on its
/// edit alone.
#[test]
fn editing_guardrail_blocks_cross_session() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;

    let file = root.path().join(format!("guarded.{MOCK_LANG}"));
    std::fs::write(&file, "fn guarded_fn()\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");

    // Two bridges, one per session.
    let mut bridge_a = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge_a.initialize()?;

    let mut bridge_b = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge_b.initialize()?;

    let ipc_path = wait_for_ipc_socket(state_home);

    // Correlate session A: PreToolUse(grep) → MCP tools/call.
    hook_roundtrip(
        &ipc_path,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": "session-a"
        }),
    )?;
    bridge_a.call_tool_text(
        "grep",
        &json!({"pattern": "guarded_fn", "directory": root_str}),
    )?;

    // Correlate session B: PreToolUse(grep) → MCP tools/call.
    hook_roundtrip(
        &ipc_path,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": "session-b"
        }),
    )?;
    bridge_b.call_tool_text(
        "grep",
        &json!({"pattern": "guarded_fn", "directory": root_str}),
    )?;

    // Session A: first edit implicitly enters editing mode and acquires
    // the per-root guardrail.
    let response_a = hook_roundtrip(
        &ipc_path,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": file.to_str().context("file path")?,
            "agent_id": "",
            "session_id": "session-a"
        }),
    )?;
    assert!(
        response_a.trim().is_empty() || !response_a.contains("deny"),
        "session A should be allowed to edit, got: {response_a}",
    );

    // Session B: first edit to the same root is blocked by the guardrail.
    let response_b = hook_roundtrip(
        &ipc_path,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": file.to_str().context("file path")?,
            "agent_id": "",
            "session_id": "session-b"
        }),
    )?;
    assert!(
        response_b.contains("Another session is editing"),
        "session B should be blocked by guardrail, got: {response_b}",
    );

    Ok(())
}

/// Sandwich correlation end-to-end: `PreToolUse` hook followed by MCP
/// `tools/call` binds the connection to the session.
#[test]
fn correlation_end_to_end() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;

    let file = root.path().join(format!("corr.{MOCK_LANG}"));
    std::fs::write(&file, "fn correlated_fn()\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");

    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;

    let ipc_path = wait_for_ipc_socket(state_home);

    // 1. Send PreToolUse hook for a Catenary tool with session_id.
    hook_roundtrip(
        &ipc_path,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": "corr-session"
        }),
    )?;

    // 2. Send MCP tools/call — this resolves correlation.
    let text = bridge.call_tool_text(
        "grep",
        &json!({"pattern": "correlated_fn", "directory": root_str}),
    )?;
    assert!(
        text.contains("correlated_fn"),
        "grep should work after correlation, got:\n{text}",
    );

    // 3. Subsequent hooks for the same session should route correctly.
    //    Verify by sending another hook on the correlated session.
    hook_roundtrip(
        &ipc_path,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "mcp_catenary_grep",
            "agent_id": "",
            "session_id": "corr-session"
        }),
    )?;

    // Verify session is still functional — another grep should work.
    let text = bridge.call_tool_text(
        "grep",
        &json!({"pattern": "correlated_fn", "directory": root_str}),
    )?;
    assert!(
        text.contains("correlated_fn"),
        "grep should work after the follow-up hook, got:\n{text}",
    );

    Ok(())
}

// ── Hookless CLI surface (bug 100) ─────────────────────────────────
//
// The documented CLI-only story: a live daemon, but no PreToolUse hook ever
// stages the diagnostics handoff. Bare `catenary diagnostics` is a fault (no
// hooked session armed a gate, so there is no debt to pay); scoped
// `catenary diagnostics <path…>` serves on-demand diagnostics without one.

/// Runs the `catenary` binary with `subargs`, isolated to `state_home`, and
/// returns `(stdout, stderr, exit_code)`.
fn run_cli(state_home: &str, subargs: &[&str]) -> Result<(String, String, Option<i32>)> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    common::isolate_env(&mut cmd, state_home);
    cmd.args(subargs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().context("run catenary binary")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

/// Root-ownership stage 3 (the bare-rerun contract retirement, deliverable 6):
/// a bare `catenary diagnostics` against an empty ledger answers `[no edited
/// files]` with exit 0 — NOT the old exit-2 "no diagnostics run staged" fault.
/// The serve reads the durable ledger by pure path algebra; an empty ledger is
/// an honest no-debt answer, not a misuse.
#[test]
fn bare_diagnostics_empty_ledger_reports_no_edited_files() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let mut bridge = BridgeProcess::spawn_in_state(state_home, |_cmd| {})?;
    bridge.initialize()?;
    let _ = wait_for_ipc_socket(state_home);

    let (stdout, stderr, code) = run_cli(state_home, &["diagnostics"])?;

    assert_eq!(
        code,
        Some(0),
        "bare diagnostics against an empty ledger is exit 0, not a fault\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stdout.contains("[no edited files]"),
        "an empty ledger answers `[no edited files]`, got:\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    Ok(())
}

/// Bug 100 ruling 2, CLI level: hookless scoped `catenary diagnostics <path…>`
/// serves on-demand diagnostics — for an explicit file and for a directory
/// argument (the `catenary diagnostics .` shape) — with exit 0 and a real
/// receipt, no handoff required.
#[test]
fn hookless_scoped_diagnostics_cli_serves_receipt() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    let file = root.path().join(format!("lint.{MOCK_LANG}"));
    std::fs::write(&file, "code\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;
    let _ = wait_for_ipc_socket(state_home);

    // Scoped file — no hook, no prepare, no editing session.
    let file_arg = file.to_str().context("file arg")?;
    let (stdout, stderr, code) = run_cli(state_home, &["diagnostics", file_arg])?;
    assert_eq!(
        code,
        Some(0),
        "hookless scoped diagnostics completes\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stdout.contains("mock diagnostic"),
        "the receipt carries the served diagnostics, got:\n{stdout}",
    );

    // Scoped directory — served hookless through the same scope router as the
    // hooked form (fan-out over the root's covered files).
    let (stdout, stderr, code) = run_cli(state_home, &["diagnostics", root_str])?;
    assert_eq!(
        code,
        Some(0),
        "hookless scoped directory diagnostics completes\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stdout.contains("mock diagnostic"),
        "the directory receipt carries the served diagnostics, got:\n{stdout}",
    );
    Ok(())
}
