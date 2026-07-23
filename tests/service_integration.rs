// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration tests for the always-on service and its spawn-on-demand
//! fallback (ws49-04).
//!
//! Two deliverables prove out here against the real `catenary` binary in an
//! isolated `XDG_*` sandbox:
//!
//! - **`SessionStart` spawn-on-demand fallback.** With no service installed, the
//!   `session-start` hook must ensure a daemon comes up on its own — today's
//!   behavior, preserved now that the MCP bridge is no longer the sole spawner
//!   and the daemon does not self-exit on idle. The daemon publishes a
//!   `state.json` snapshot with its pid; we assert that appears.
//! - **`catenary service install` / `uninstall` / `status`.** On Linux the unit
//!   is written under the ISOLATED config base (`isolate_env` points
//!   `CATENARY_CONFIG_DIR` at `<root>/config`), never the operator's real
//!   `~/.config/systemd`. We assert the unit *content* (the `ExecStart`, the
//!   `MALLOC_ARENA_MAX=2` arena cap with its ws49-02 citation) and the
//!   install→uninstall lifecycle on disk. The live `systemctl --user` leg is
//!   environment-gated — a CI sandbox has no user bus — so it is NOT asserted;
//!   the durable artifact is the file, and `install` reports the live leg's
//!   outcome without failing on it.

#![cfg(target_os = "linux")]
#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use common::{DaemonGuard, isolate_env, xdg_config_home};

/// Runs `catenary service <sub>` in the isolated sandbox at `state_home`,
/// returning the captured stdout+stderr and the exit success flag.
fn run_service(state_home: &str, sub: &str) -> (String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.args(["service", sub])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().expect("run catenary service");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (combined, out.status.success())
}

/// The isolated systemd `--user` unit path for the sandbox at `state_home`.
fn unit_path(state_home: &str) -> std::path::PathBuf {
    xdg_config_home(state_home)
        .join("systemd")
        .join("user")
        .join("catenary.service")
}

/// `catenary service install` writes the systemd `--user` unit under the
/// ISOLATED config base — never the operator's real `~/.config/systemd` — and
/// the unit carries the daemon `ExecStart` plus the `MALLOC_ARENA_MAX=2` arena
/// cap with its ws49-02 measured citation. The live `systemctl --user` leg is
/// environment-gated (no user bus in a CI sandbox) and reported, not fatal, so
/// `install` still exits 0.
#[test]
fn service_install_writes_isolated_unit_with_arena_cap() {
    let state_dir = tempfile::tempdir().expect("state dir");
    let state_home = state_dir.path().to_str().expect("state home");

    let path = unit_path(state_home);
    assert!(!path.exists(), "no unit before install");

    let (output, ok) = run_service(state_home, "install");
    assert!(
        ok,
        "install must exit 0 even when the user bus is absent; output:\n{output}",
    );
    assert!(path.is_file(), "unit must be written at {}", path.display());

    let unit = std::fs::read_to_string(&path).expect("read unit");
    assert!(
        unit.contains("ExecStart=") && unit.contains(" daemon"),
        "unit must exec the daemon:\n{unit}",
    );
    assert!(
        unit.contains("Environment=MALLOC_ARENA_MAX=2"),
        "unit must carry the arena cap:\n{unit}",
    );
    assert!(
        unit.contains("82 of 94 MiB") && unit.contains("bug 136"),
        "unit must cite the ws49-02 arena receipt and bug 136:\n{unit}",
    );

    // Poison guard: the operator's real unit dir is never touched.
    let real = dirs::config_dir().map(|d| d.join("systemd/user/catenary.service"));
    if let Some(real) = real {
        // We cannot assert absence (the operator may genuinely have installed
        // one), but the isolated ExecStart must name the TEST binary, proving
        // the write landed in the sandbox, not the real path.
        assert!(
            unit.contains(env!("CARGO_BIN_EXE_catenary")),
            "isolated unit must exec the test binary, not a real install ({}):\n{unit}",
            real.display(),
        );
    }
}

/// `service uninstall` removes the unit cleanly and is idempotent: a second
/// uninstall on an already-clean sandbox still exits 0 and reports the absence.
#[test]
fn service_uninstall_removes_the_unit_and_is_idempotent() {
    let state_dir = tempfile::tempdir().expect("state dir");
    let state_home = state_dir.path().to_str().expect("state home");

    let (_out, ok) = run_service(state_home, "install");
    assert!(ok, "install must exit 0");
    let path = unit_path(state_home);
    assert!(path.is_file(), "unit present after install");

    let (out1, ok1) = run_service(state_home, "uninstall");
    assert!(ok1, "uninstall must exit 0; output:\n{out1}");
    assert!(!path.exists(), "unit removed after uninstall");

    // Idempotent: nothing left to remove, still clean exit.
    let (out2, ok2) = run_service(state_home, "uninstall");
    assert!(ok2, "second uninstall must exit 0; output:\n{out2}");
    assert!(
        out2.contains("already removed") || out2.contains("uninstalled"),
        "second uninstall must report the clean state:\n{out2}",
    );
}

/// `service status` is honest in both states: it reports "not installed" on a
/// fresh sandbox and "installed" after an install, naming the systemd manager
/// either way.
#[test]
fn service_status_honest_in_both_states() {
    let state_dir = tempfile::tempdir().expect("state dir");
    let state_home = state_dir.path().to_str().expect("state home");

    let (before, ok_before) = run_service(state_home, "status");
    assert!(ok_before, "status must exit 0; output:\n{before}");
    assert!(
        before.contains("systemd --user"),
        "status names the manager:\n{before}",
    );
    assert!(
        before.contains("Installed: no"),
        "status must report not-installed on a fresh sandbox:\n{before}",
    );

    let (_out, ok) = run_service(state_home, "install");
    assert!(ok, "install must exit 0");

    let (after, ok_after) = run_service(state_home, "status");
    assert!(ok_after, "status must exit 0; output:\n{after}");
    assert!(
        after.contains("Installed: yes"),
        "status must report installed after install:\n{after}",
    );
}

/// `SessionStart` spawn-on-demand fallback (ws49-04, regression): with NO service
/// installed and NO daemon running, `catenary hook session-start` must bring a
/// daemon up on its own — the daemon publishes a `state.json` snapshot carrying
/// its pid. This preserves today's behavior now that the daemon no longer
/// self-exits on idle and the MCP bridge is no longer the sole spawner.
#[test]
fn session_start_ensures_daemon_when_no_service_installed() {
    let state_dir = tempfile::tempdir().expect("state dir");
    let state_home = state_dir.path().to_str().expect("state home");

    // The guard tears any spawned daemon down on drop (bug 131 discipline).
    let guard = DaemonGuard::new(state_home);

    // No service installed: the fresh sandbox has no unit file, so
    // `is_installed()` is false inside the hook and the fallback engages.
    assert!(
        !unit_path(state_home).exists(),
        "sandbox must start with no service installed",
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.args(["hook", "session-start", "--format=claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let payload = json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-service-fallback",
        "source": "startup",
        "cwd": state_dir.path().to_str().expect("cwd"),
    });
    let mut child = cmd.spawn().expect("spawn session-start hook");
    {
        let mut stdin = child.stdin.take().expect("hook stdin");
        stdin
            .write_all(payload.to_string().as_bytes())
            .expect("write hook stdin");
    }
    let out = child.wait_with_output().expect("wait for hook");
    assert!(
        out.status.success(),
        "session-start hook must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );

    // The fallback spawned a daemon: it publishes a pid in its snapshot.
    let pid = guard
        .daemon_pid()
        .expect("SessionStart with no service must ensure a daemon (spawn-on-demand fallback)");
    assert!(
        daemon_alive_within(pid, Duration::from_secs(5)),
        "spawned daemon must be alive"
    );
}

/// Whether `pid` is alive at any point within `window` (a freshly spawned
/// daemon may still be finishing boot when its snapshot first appears).
fn daemon_alive_within(pid: u32, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    loop {
        if common::pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
