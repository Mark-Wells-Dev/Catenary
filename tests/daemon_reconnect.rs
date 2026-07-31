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
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
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
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);

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
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
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
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
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

// ── The lifecycle verbs and the intent marker (pulse 04) ─────────────

/// The daemon intent marker path under a test state home:
/// `runtime_dir()/daemon.intent`.
fn intent_marker(state_home: &str) -> PathBuf {
    xdg_runtime_dir(state_home).join("daemon.intent")
}

/// Reads the marker's mode word (its first line), or `None` when absent.
fn intent_mode(state_home: &str) -> Option<String> {
    let content = std::fs::read_to_string(intent_marker(state_home)).ok()?;
    content.lines().next().map(str::trim).map(str::to_string)
}

/// Pulse 04: `catenary stop` records the `stop` intent (stop means stop —
/// bridges wait instead of respawning), and `catenary start` — the one resume
/// verb — clears it before bringing the daemon back.
#[test]
fn stop_records_stop_intent_and_start_clears_it() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
    let (ipc_sock, mcp_sock) = socket_paths(&state_home);

    let up = run_catenary(&state_home, &["start"])?;
    assert!(up.status.success(), "start must exit 0");
    assert!(ipc_sock.exists(), "daemon up after start");
    assert_eq!(
        intent_mode(&state_home),
        None,
        "no intent marker while the daemon runs",
    );

    let stop = run_catenary(&state_home, &["stop", "--force"])?;
    assert!(stop.status.success(), "stop must exit 0");
    assert_eq!(
        intent_mode(&state_home).as_deref(),
        Some("stop"),
        "stop must leave the `stop` intent on disk",
    );
    let out = String::from_utf8_lossy(&stop.stdout);
    assert!(
        out.contains("staying stopped"),
        "stop teaches the new semantics, got:\n{out}",
    );
    assert!(wait_gone(&mcp_sock), "sockets removed after stop");

    // `start` is the one resume verb: it clears the marker and brings the
    // daemon back.
    let resume = run_catenary(&state_home, &["start"])?;
    assert!(resume.status.success(), "resume start must exit 0");
    assert_eq!(
        intent_mode(&state_home),
        None,
        "start must clear the stop intent",
    );
    assert!(ipc_sock.exists(), "daemon back up after start");

    let _ = run_catenary(&state_home, &["stop", "--force"]);
    Ok(())
}

/// Pulse 04, census-zero leg: `catenary restart` produces a running daemon
/// when none was running, and clears a leftover stop marker so the bounce
/// cannot be misread as a declared outage.
#[test]
fn restart_starts_a_daemon_at_census_zero_and_clears_leftover_marker() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
    let (ipc_sock, _mcp_sock) = socket_paths(&state_home);

    // Seed a leftover `stop` marker (e.g. a `stop` whose `start` never came).
    let marker = intent_marker(&state_home);
    std::fs::create_dir_all(marker.parent().context("marker parent")?)?;
    std::fs::write(&marker, "stop\n2026-07-17T00:00:00Z\n")?;
    assert!(!ipc_sock.exists(), "no daemon before restart");

    let restart = run_catenary(&state_home, &["restart"])?;
    assert!(
        restart.status.success(),
        "restart must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&restart.stderr),
    );
    let out = String::from_utf8_lossy(&restart.stdout);
    assert!(
        out.contains("No daemon was running"),
        "census-zero restart names the missing old daemon, got:\n{out}",
    );
    assert!(
        out.contains("Daemon started"),
        "restart starts the new daemon itself, got:\n{out}",
    );
    assert!(ipc_sock.exists(), "restart must produce a running daemon");
    assert_eq!(
        intent_mode(&state_home),
        None,
        "restart must leave no marker (a leftover one is cleared)",
    );

    let _ = run_catenary(&state_home, &["stop", "--force"]);
    Ok(())
}

/// Pulse 04: `catenary restart` bounces a running daemon — old one stopped,
/// new one started — and leaves no intent marker behind.
#[test]
fn restart_bounces_a_running_daemon_without_a_marker() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
    let (ipc_sock, _mcp_sock) = socket_paths(&state_home);

    let up = run_catenary(&state_home, &["start"])?;
    assert!(up.status.success(), "start must exit 0");
    assert!(ipc_sock.exists(), "daemon up before restart");

    let restart = run_catenary(&state_home, &["restart"])?;
    assert!(
        restart.status.success(),
        "restart must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&restart.stderr),
    );
    let out = String::from_utf8_lossy(&restart.stdout);
    assert!(
        out.contains("Daemon stopped"),
        "restart reports the old daemon stopped, got:\n{out}",
    );
    assert!(
        out.contains("Daemon started"),
        "restart reports the new daemon started, got:\n{out}",
    );
    assert!(ipc_sock.exists(), "a daemon must be running after restart");
    assert_eq!(
        intent_mode(&state_home),
        None,
        "restart writes no intent marker",
    );

    let _ = run_catenary(&state_home, &["stop", "--force"]);
    Ok(())
}

/// Pulse 04: `catenary quit` records the `quit` intent before the daemon
/// dies, and its output names the consequence (failed MCP server until
/// `catenary start` plus a fresh session).
#[test]
fn quit_records_quit_intent() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    // Panic-safe daemon teardown (bug 131): these tests bounce/kill daemons
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
    let (ipc_sock, mcp_sock) = socket_paths(&state_home);

    let up = run_catenary(&state_home, &["start"])?;
    assert!(up.status.success(), "start must exit 0");
    assert!(ipc_sock.exists(), "daemon up before quit");

    let quit = run_catenary(&state_home, &["quit", "--force"])?;
    assert!(
        quit.status.success(),
        "quit must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&quit.stderr),
    );
    assert_eq!(
        intent_mode(&state_home).as_deref(),
        Some("quit"),
        "quit must leave the `quit` intent on disk",
    );
    assert!(wait_gone(&mcp_sock), "sockets removed after quit");

    // `start` resumes from a quit exactly as from a stop: marker cleared,
    // daemon up.
    let resume = run_catenary(&state_home, &["start"])?;
    assert!(resume.status.success(), "start after quit must exit 0");
    assert_eq!(
        intent_mode(&state_home),
        None,
        "start must clear the quit intent",
    );

    let _ = run_catenary(&state_home, &["stop", "--force"]);
    Ok(())
}

/// Whether process `pid` is still alive (`kill -0`, no signal delivered).
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

/// Waits (attempt-bounded backstop) for process `pid` to exit; returns whether
/// it did.
fn wait_pid_gone(pid: u32) -> bool {
    let backstop = Instant::now() + Duration::from_secs(15);
    while pid_alive(pid) {
        if Instant::now() >= backstop {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Returns the contributor sources `roots-ls` reports for `target`, or `None`
/// when `target` is not on the board at all.
fn roots_ls_sources(socket: &Path, target: &str) -> Result<Option<Vec<String>>> {
    let resp = common::ipc_request(socket, &json!({ "method": "tool/roots-ls" }))?;
    let roots: serde_json::Value = serde_json::from_str(resp.trim()).context("roots-ls json")?;
    Ok(roots["roots"].as_array().and_then(|arr| {
        arr.iter()
            .find(|e| e["path"].as_str() == Some(target))
            .and_then(|e| e["sources"].as_array())
            .map(|s| {
                s.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
    }))
}

/// Polls `roots-ls` until `target` carries an `mcp:` contributor, returning the
/// sources it settled on — or the last read at the backstop, so the caller's
/// assertion (never this helper) reports the failure.
///
/// Tolerates transport errors while polling: a daemon mid-respawn refuses the
/// IPC connect, which is a not-yet, not a verdict.
fn wait_for_mcp_sources(socket: &Path, target: &str) -> Vec<String> {
    let backstop = Instant::now() + Duration::from_mins(2);
    loop {
        let sources = roots_ls_sources(socket, target)
            .ok()
            .flatten()
            .unwrap_or_default();
        if sources.iter().any(|s| s.starts_with("mcp:")) || Instant::now() >= backstop {
            return sources;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// misc 169: a session that reattaches to a fresh daemon re-anchors its roots.
///
/// The pinned specimen (three live sessions across a `catenary stop` +
/// `systemctl --user restart`): every bridge reattached and replayed
/// `initialize` — the streams even log "Client supports roots capability" — and
/// **no `roots/list` round ever followed**, so the board read `No tracked roots`
/// under three live connections, and each session lived on root-orphaned until a
/// manual `/mcp`. The cause: the roots trigger hung off
/// `notifications/initialized`, a once-per-client-start notification the replay
/// never re-sends. It now hangs off `initialize` itself, which the replay does
/// send — so a replayed init and a fresh init get identical treatment.
///
/// SIGKILL is the reproducer here (the clean-stop path reaches the same replay
/// through a longer route); what matters is that the fresh daemon asks, and that
/// the answer lands under the reattached connection's own session key.
#[test]
fn reattached_session_re_anchors_its_roots() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    // Panic-safe daemon teardown (bug 131): this test kills a daemon
    // deliberately; the guard covers the assertion-failure exits in between.
    let _daemon_guard = common::DaemonGuard::new(&state_home);
    let (ipc_sock, mcp_sock) = socket_paths(&state_home);

    let root_dir = common::canonical_tempdir()?;
    let root = root_dir.path().to_str().context("root path")?.to_string();

    // Session up with roots declared over MCP — daemon #1 anchors them under
    // its `mcp:` session key.
    let mut bridge = BridgeProcess::spawn_in_state(&state_home, |_cmd| {})?;
    bridge.initialize_with_roots(&[&root])?;
    let before = wait_for_mcp_sources(&ipc_sock, &root);
    assert!(
        before.iter().any(|s| s.starts_with("mcp:")),
        "daemon #1 must anchor the declared root under an `mcp:` contributor, got: {before:?}",
    );

    // Kill it. The bridge reconnects, respawns a daemon through the same
    // single-instance path, and replays the captured `initialize` against it —
    // the specimen's exact shape, with no host involvement at all.
    let pid = bridge.daemon_pid().context("daemon pid before kill")?;
    sigkill(pid)?;
    let fresh_pid = wait_for_respawn(&state_home, pid).context("bridge should respawn a daemon")?;
    assert_ne!(fresh_pid, pid, "the respawned daemon must be a new process");
    assert!(
        mcp_sock.exists(),
        "respawned daemon should re-bind its MCP socket"
    );

    // The fix: the replayed `initialize` makes the fresh daemon ask for roots
    // back through the still-live connection. The bridge swallows the replayed
    // initialize response, so this request is the first thing the host sees.
    // Bounded read — under the old behavior nothing is ever sent, and a bare
    // `recv()` would hang the suite instead of failing here.
    let asked = bridge
        .recv_timeout(Duration::from_secs(30))?
        .context("the fresh daemon never asked for roots after the replayed initialize")?;
    assert_eq!(
        asked.get("method").and_then(serde_json::Value::as_str),
        Some("roots/list"),
        "expected a roots/list request after the replayed initialize, got: {asked:?}",
    );
    let request_id = asked
        .get("id")
        .context("roots/list request missing id")?
        .clone();

    // Answer exactly as the host would.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": { "roots": [{"uri": format!("file://{root}")}] }
    }))?;

    // The board carries the root again, under the FRESH daemon's session key
    // (bug 147: `mcp:{conn_id}` — the same key its disconnect cleanup uses).
    let after = wait_for_mcp_sources(&ipc_sock, &root);
    assert!(
        after.iter().any(|s| s.starts_with("mcp:")),
        "the reattached session must re-anchor its root under an `mcp:` \
         contributor on the fresh daemon, got: {after:?}",
    );

    drop(bridge);
    Ok(())
}

/// Stop-loss (the 2026-07-20 sighting): a graceful `catenary stop` issued while
/// a bridge session is attached must end the daemon *process* — its exit is
/// what closes the accepted MCP connection, and that close is the only signal
/// the bridge's reader reacts to (`proxy_with_reconnect` triggers on EOF/error
/// alone). The wedge under test: `handle_mcp_connection` runs the MCP loop as a
/// blocking `read` inside `spawn_blocking`; after the accept loop returns,
/// `serve_daemon` drops the runtime, and `Runtime::drop` joins in-flight
/// blocking tasks — so the daemon waits on the bridge to hang up while the
/// bridge waits on the daemon's EOF. Neither ever fires; the daemon lingers as
/// a zombie holding the connection, and the bridge never reattaches (the board
/// shows `mcp_connections: 0` forever). SIGKILL coverage above never catches
/// this: the kernel closes a killed daemon's fds for free.
#[test]
fn stopped_daemon_exits_while_bridge_attached() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    // Panic-safe daemon teardown (bug 131): the guard reaps the lingering
    // daemon this test exists to expose, once the assertion has fired.
    let _daemon_guard = common::DaemonGuard::new(&state_home);

    // Session up — daemon spawned by the bridge init, connection held.
    let mut bridge = BridgeProcess::spawn_in_state(&state_home, |_cmd| {})?;
    bridge.initialize()?;
    bridge.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))?;
    let answered = bridge.recv()?;
    assert!(
        answered.get("result").is_some(),
        "ping before the stop should be answered, got: {answered:?}",
    );
    let pid = bridge.daemon_pid().context("daemon pid before stop")?;

    // Graceful stop with the bridge still attached — the make-install shape.
    let stop = run_catenary(&state_home, &["stop", "--force"])?;
    assert!(
        stop.status.success(),
        "stop must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&stop.stderr),
    );
    let stdout = String::from_utf8_lossy(&stop.stdout);
    assert!(
        stdout.contains("Daemon stopped"),
        "stop must report the daemon stopped, got:\n{stdout}",
    );

    // The daemon process must actually exit: its exit closes the accepted MCP
    // socket, which is the bridge's ONLY daemon-loss signal. A daemon that
    // lingers here strands the session permanently (stop-loss wedge).
    assert!(
        wait_pid_gone(pid),
        "stopped daemon (pid {pid}) still alive with a bridge attached — the \
         blocking MCP read pins Runtime::drop, the accepted connection never \
         closes, and the bridge's EOF-triggered reconnect can never fire",
    );

    drop(bridge);
    Ok(())
}
