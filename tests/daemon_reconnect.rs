// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration coverage for bridge reconnect/respawn and `catenary start`
//! (bug 80, legs 1 and 2).
//!
//! Bug 80: a killed daemon stranded every live session — the bridge kept the
//! host↔bridge stdio link up (host reports "connected") while the bridge↔daemon
//! socket behind it was dead, and nothing respawned the daemon. These tests pin
//! the fixes:
//!
//! - **Leg 1**: after `catenary stop` kills the daemon mid-session, a fresh MCP
//!   request through the bridge triggers a transparent reconnect — the bridge
//!   respawns the daemon (same single-instance path), replays `initialize`, and
//!   the request gets a real answer. Under the old behavior the bridge would
//!   EOF and the request would never be answered.
//! - **Leg 2**: `catenary start` brings the daemon up idempotently.
//!
//! All assertions are on *work performed* (a real ping answer, a live socket, a
//! reported outcome), never on wall-clock timing — the reconnect backoff is
//! attempt-structured and not test-observable.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use common::{BridgeProcess, isolate_env, xdg_runtime_dir, xdg_state_home};

/// The daemon's IPC and MCP socket paths under a state home.
fn socket_paths(state_home: &str) -> (PathBuf, PathBuf) {
    let dir = xdg_state_home(state_home).join("catenary");
    (dir.join("catenary.sock"), dir.join("catenary-mcp.sock"))
}

/// Runs `catenary <args>` isolated to `state_home`, returning its output.
fn run_catenary(state_home: &str, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.args(args);
    cmd.output().context("run catenary")
}

/// Waits (attempt-bounded, not a semantic wall-clock) for `path` to stop
/// existing, then returns whether it is gone.
fn wait_gone(path: &Path) -> bool {
    let backstop = Instant::now() + Duration::from_secs(10);
    while path.exists() {
        if Instant::now() >= backstop {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Reads the daemon's pid from its `state.json` snapshot, or `None`.
fn read_daemon_pid(state_home: &str) -> Option<u32> {
    let state_json = xdg_runtime_dir(state_home)
        .join("catenary")
        .join("state.json");
    let text = std::fs::read_to_string(&state_json).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let pid = value.get("daemon")?.get("pid")?.as_u64()?;
    u32::try_from(pid).ok().filter(|&p| p != 0)
}

/// Waits (attempt-bounded backstop) until a daemon with a pid *different* from
/// `old_pid` publishes its `state.json` — the "the bridge respawned a fresh
/// daemon" signal. A distinct pid proves the reconnect reached a genuinely new
/// process, not the dying original.
fn wait_for_respawn(state_home: &str, old_pid: u32) -> Option<u32> {
    // Generous backstop (matches `common::POLL_BACKSTOP`): under the maintainer's
    // heavy multi-agent load a respawn + snapshot publish can lag well past a few
    // seconds. This only trips on a genuine hang, never the happy path.
    let backstop = Instant::now() + Duration::from_mins(2);
    loop {
        if let Some(pid) = read_daemon_pid(state_home)
            && pid != old_pid
        {
            return Some(pid);
        }
        if Instant::now() >= backstop {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGKILLs the daemon process `pid` — the bug-80 sighting is a killed daemon.
///
/// SIGKILL (not the catchable SIGTERM/`catenary stop` graceful path) force-closes
/// the daemon's fds immediately, so the bridge's MCP read gets EOF and the
/// reconnect state machine fires. Because SIGKILL is uncatchable, the daemon runs
/// **no** cleanup: its socket files linger as *stale* entries (a dead listener),
/// which `connect_or_start_daemon` clears before respawning — exactly the
/// stale-socket recovery the init path already owns. Uses `kill(1)` via the
/// inherited PATH — this helper is test-side and does not go through
/// `isolate_env` (which blanks PATH for the spawned catenary subprocesses only).
fn sigkill(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .context("run kill -KILL")?;
    anyhow::ensure!(status.success(), "kill -KILL {pid} failed");
    Ok(())
}

/// Leg 1: a daemon killed mid-session reconnects transparently — a `ping` sent
/// after the kill is answered by a respawned daemon, instead of the bridge
/// EOF-ing (the bug-80 orphan state).
#[test]
fn killed_daemon_reconnects_and_answers_ping() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    let (_ipc_sock, mcp_sock) = socket_paths(&state_home);

    // Session up — daemon #1 spawned by the bridge init.
    let mut bridge = BridgeProcess::spawn_in_state(&state_home, |_cmd| {})?;
    bridge.initialize()?;
    assert!(
        mcp_sock.exists(),
        "daemon MCP socket should exist after init"
    );

    // A ping through the live session is answered by daemon #1.
    bridge.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))?;
    let first = bridge.recv()?;
    assert!(
        first.get("result").is_some(),
        "ping before the kill should be answered, got: {first:?}",
    );

    // Kill the daemon — the bug-80 sighting: `kill <daemon-pid>`. SIGKILL
    // force-closes its fds, so the bridge's MCP read errors (ECONNRESET) and the
    // reconnect state machine fires: it respawns the daemon through the same
    // single-instance path and replays initialize.
    let pid = bridge.daemon_pid().context("daemon pid before kill")?;
    sigkill(pid)?;

    // The bridge respawns the daemon transparently — wait for a genuinely fresh
    // daemon (a different pid) to publish its snapshot. This settles the
    // reconnect before the next request, so the ping is not lost into the dying
    // original's socket (a host would resend that in-flight request; the test
    // synchronizes on the respawn instead).
    let fresh_pid = wait_for_respawn(&state_home, pid).context("bridge should respawn a daemon")?;
    assert_ne!(fresh_pid, pid, "the respawned daemon must be a new process");
    assert!(
        mcp_sock.exists(),
        "respawned daemon should re-bind its MCP socket"
    );

    // Leg 1: with the daemon back, a ping through the SAME session — never
    // reconnected by the host — is answered by the respawned daemon. Under the
    // old behavior the bridge would have EOF-ed on the kill and this `recv()`
    // would bail; here the channel survived the daemon's death.
    bridge.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))?;
    let second = bridge
        .recv()
        .context("ping after daemon kill should be answered by the respawned daemon")?;
    assert!(
        second.get("result").is_some(),
        "post-kill ping should be answered by the respawned daemon, got: {second:?}",
    );
    assert_eq!(
        second.get("id").and_then(serde_json::Value::as_i64),
        Some(2),
        "the answer must be to the post-kill ping (id 2), got: {second:?}",
    );

    drop(bridge);
    Ok(())
}

/// Leg 1 companion: after the reconnect, the search surface works again over the
/// respawned daemon — `catenary grep` connects to the fresh IPC socket and
/// serves results, so a stopped daemon needs no per-session restart to recover
/// tooling.
#[test]
fn search_surface_recovers_after_reconnect() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();

    let tree = tempfile::tempdir()?;
    std::fs::write(tree.path().join("hay.txt"), "a needle in here\n")?;

    let mut bridge = BridgeProcess::spawn_in_state(&state_home, |_cmd| {})?;
    bridge.initialize()?;

    // Kill the daemon (SIGKILL — force-close its fds). The bridge's reader errors
    // and the reconnect state machine respawns the daemon transparently.
    let pid = bridge.daemon_pid().context("daemon pid before kill")?;
    sigkill(pid)?;

    // Wait for the bridge to respawn a genuinely fresh daemon (a new pid).
    let fresh_pid = wait_for_respawn(&state_home, pid).context("bridge should respawn a daemon")?;
    assert_ne!(fresh_pid, pid, "the respawned daemon must be a new process");

    // The search surface is back: `catenary grep` reaches the fresh daemon.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, &state_home);
    cmd.current_dir(tree.path()).args(["grep", "needle"]);
    let out = cmd.output().context("run catenary grep post-reconnect")?;
    assert!(out.status.success(), "grep post-reconnect must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("needle"),
        "grep must serve results over the respawned daemon, got:\n{stdout}",
    );
    // Daemon-served, so the daemon-less marker must be absent.
    assert!(
        !stderr.contains("no daemon"),
        "grep reached a live daemon — no daemon-less marker, got:\n{stderr}",
    );

    drop(bridge);
    Ok(())
}

/// Leg 2: `catenary start` brings the daemon up from nothing, then is
/// idempotent — a second `start` reports it is already running.
#[test]
fn catenary_start_brings_daemon_up_idempotently() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    let (ipc_sock, _mcp_sock) = socket_paths(&state_home);

    assert!(!ipc_sock.exists(), "no daemon before start");

    // First start: from nothing.
    let first = run_catenary(&state_home, &["start"])?;
    assert!(
        first.status.success(),
        "catenary start must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&first.stderr),
    );
    let first_out = String::from_utf8_lossy(&first.stdout);
    assert!(
        first_out.contains("Daemon started"),
        "first start should report a fresh daemon, got:\n{first_out}",
    );
    assert!(
        ipc_sock.exists(),
        "daemon IPC socket should exist after start"
    );

    // Second start: idempotent — already running.
    let second = run_catenary(&state_home, &["start"])?;
    assert!(second.status.success(), "idempotent start must exit 0");
    let second_out = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_out.contains("already running"),
        "second start should report the daemon already up, got:\n{second_out}",
    );

    // Clean up the standalone daemon (no bridge holds it).
    let _ = run_catenary(&state_home, &["stop", "--force"]);
    Ok(())
}

/// Leg 2 recovery: `catenary start` after a `catenary stop` brings the daemon
/// back — the one-command remedy bug 80 called for.
#[test]
fn catenary_start_recovers_a_stopped_daemon() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    let (ipc_sock, mcp_sock) = socket_paths(&state_home);

    // Bring one up and stop it.
    let up = run_catenary(&state_home, &["start"])?;
    assert!(up.status.success(), "start must exit 0");
    assert!(ipc_sock.exists(), "daemon up after start");

    let stop = run_catenary(&state_home, &["stop", "--force"])?;
    assert!(stop.status.success(), "stop must exit 0");
    assert!(wait_gone(&mcp_sock), "sockets removed after stop");

    // Start again — recovers.
    let again = run_catenary(&state_home, &["start"])?;
    assert!(again.status.success(), "recovery start must exit 0");
    let out = String::from_utf8_lossy(&again.stdout);
    assert!(
        out.contains("Daemon started"),
        "start after stop should report a fresh daemon, got:\n{out}",
    );
    assert!(ipc_sock.exists(), "daemon back up after recovery start");

    let _ = run_catenary(&state_home, &["stop", "--force"]);
    Ok(())
}
