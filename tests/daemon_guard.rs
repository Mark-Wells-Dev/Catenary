// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The daemon-teardown guard actually tears down (bug 131).
//!
//! Integration tests spawn real daemons detached in a new process group, so a
//! test that ends without a stop step leaks its daemon forever — the daemon's
//! exit is disconnect-event-driven, and a daemon that never sees an MCP
//! connection never arms it. `common::DaemonGuard` is the panic-safe teardown;
//! these tests pin that a dropped guard kills exactly that leak class, and that
//! it is a silent no-op when no daemon ever ran.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use common::{DaemonGuard, POLL_SPACING, isolate_env, pid_alive};

/// A dropped guard terminates a daemon nobody stopped.
///
/// `catenary start` spawns the daemon detached and probes it over IPC only —
/// the daemon never sees an MCP connection, so its last-disconnect exit never
/// arms: without the guard it would outlive this test indefinitely (the exact
/// bug-131 leak class the recovery `ps` found, six daemons deep). Dropping the
/// guard — the same thing a panic unwind does — must kill it.
#[test]
fn dropped_guard_kills_a_daemon_no_one_stopped() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let guard = DaemonGuard::new(state_home);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.arg("start");
    let out = cmd.output().context("run catenary start")?;
    assert!(
        out.status.success(),
        "catenary start must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );

    let pid = guard
        .daemon_pid()
        .context("started daemon must publish its pid in the snapshot")?;
    assert!(
        pid_alive(pid),
        "daemon (pid {pid}) must be alive before the guard drops"
    );

    // Simulate a test ending WITHOUT an explicit stop — the leak scenario.
    drop(guard);

    // The guard SIGTERMs and escalates to SIGKILL within its grace, so the
    // process must be gone well within this backstop. (It was detached from
    // its spawner, so init reaps it — no zombie ambiguity for `kill -0`.)
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "dropping the guard must terminate the daemon (pid {pid} still alive)"
        );
        std::thread::sleep(POLL_SPACING);
    }
    Ok(())
}

/// A guard over a state root where no daemon ever ran is a silent no-op:
/// absent snapshot, absent log, nothing to signal — and no panic in `Drop`.
#[test]
fn guard_is_a_silent_no_op_when_no_daemon_ever_ran() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let guard = DaemonGuard::new(state_dir.path());
    drop(guard);
    Ok(())
}
