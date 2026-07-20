// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Birth grace: a daemon that never sees a client exits (bug 131, daemon leg).
//!
//! The daemon's exit lifecycle is disconnect-event-driven, so a daemon born
//! into silence — census zero from birth — used to be immortal: nothing ever
//! armed its exit (the leak class behind the field census of hundreds of test
//! daemons and the two-day accidental-spawn ghost). These tests pin the fix
//! end-to-end against real spawned daemons: a never-served daemon exits
//! within the injected window with a clean firehose record; one served IPC
//! dispatch inside the window retires the grace for good; and the ordinary
//! bridge lifecycle is untouched. The window is injected across the process
//! boundary via `CATENARY_BIRTH_GRACE_SECS` (inherited by the daemon the
//! `catenary start` ceremony spawns).

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use common::{BridgeProcess, DaemonGuard, POLL_SPACING, isolate_env, pid_alive, xdg_cache_home};

/// The tiny birth-grace window injected into spawned daemons, in seconds.
const WINDOW_SECS: u64 = 2;
const WINDOW: Duration = Duration::from_secs(WINDOW_SECS);

/// Starts a daemon via `catenary start` with the injected birth-grace window.
///
/// `catenary start` probes the daemon with bare IPC connects only (the spawn
/// ceremony), so a daemon it starts has still never been *served* — exactly
/// the census-zero-from-birth shape under test. The daemon child inherits
/// `CATENARY_BIRTH_GRACE_SECS` from the `start` process.
fn start_daemon(state_home: &str) -> Result<()> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.env("CATENARY_BIRTH_GRACE_SECS", WINDOW_SECS.to_string());
    cmd.arg("start");
    let out = cmd.output().context("run catenary start")?;
    anyhow::ensure!(
        out.status.success(),
        "catenary start must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
}

/// Concatenates every instance-trace firehose file (`trace.jsonl` and its
/// rotated segments) under the isolated cache dir. Daemon-lifecycle records
/// land on the instance trace stream at `<cache>/catenary/<instance>/`.
fn read_trace_firehose(state_home: &str) -> String {
    fn walk(dir: &Path, buf: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, buf);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("trace.jsonl"))
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                buf.push_str(&text);
            }
        }
    }
    let mut buf = String::new();
    walk(&xdg_cache_home(state_home).join("catenary"), &mut buf);
    buf
}

/// A daemon given no service exits on its own within the birth grace (plus
/// scheduling margin), leaving a clean `info!` firehose record — routine
/// housekeeping, not a crash and not an interrupt.
#[test]
fn never_served_daemon_exits_within_the_birth_grace() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let guard = DaemonGuard::new(state_home);
    start_daemon(state_home)?;

    let pid = guard
        .daemon_pid()
        .context("started daemon must publish its pid in the snapshot")?;

    // The exit is self-driven: no stop, no signal, no client. Generous
    // margin over the 2 s window so external CPU contention never flakes
    // this; the window's effect is pinned by the firehose record below
    // (the production window is 10 minutes, far beyond this backstop).
    let deadline = Instant::now() + Duration::from_mins(1);
    while pid_alive(pid) {
        anyhow::ensure!(
            Instant::now() < deadline,
            "a never-served daemon (pid {pid}) must exit on its own within the birth grace",
        );
        std::thread::sleep(POLL_SPACING);
    }

    let firehose = read_trace_firehose(state_home);
    anyhow::ensure!(
        firehose.contains("never served a client within the birth grace"),
        "the birth-grace exit must leave its firehose record; trace stream was:\n{firehose}",
    );
    Ok(())
}

/// One real CLI dispatch inside the window is service: the birth grace
/// retires permanently and the daemon survives well past the window. This is
/// the deliberate hookful-but-bridgeless shape (`catenary start`, zero MCP
/// bridges) that the fix must preserve.
#[test]
fn one_cli_dispatch_inside_the_window_keeps_the_daemon_alive() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let guard = DaemonGuard::new(state_home);
    start_daemon(state_home)?;

    // Dispatch immediately after `start` returns: the daemon's sockets are
    // bound before its accept loop arms the window, so a request issued now
    // is parsed at (or moments after) the window's start — never after its
    // expiry. `catenary roots` rides the same IPC dispatch path as every
    // hook/CLI request.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.arg("roots");
    let out = cmd.output().context("run catenary roots")?;
    anyhow::ensure!(
        out.status.success(),
        "catenary roots must be served, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );

    let pid = guard
        .daemon_pid()
        .context("started daemon must publish its pid in the snapshot")?;

    // Served once — the daemon must outlive the window with room to spare
    // (3x). Poll rather than sleep-then-check so an early death fails at
    // the moment it happens.
    let end = Instant::now() + WINDOW * 3;
    while Instant::now() < end {
        anyhow::ensure!(
            pid_alive(pid),
            "a served daemon (pid {pid}) must survive birth-grace expiry",
        );
        std::thread::sleep(POLL_SPACING);
    }
    // The guard tears the surviving daemon down on drop.
    Ok(())
}

/// The normal bridge lifecycle is unaffected: an MCP bridge is service, so a
/// bridged daemon sails past the (tiny) birth window, serves its session,
/// and the bridge shuts down cleanly — the disconnect-driven lifecycle owns
/// the daemon from there.
#[test]
fn bridge_lifecycle_is_unaffected_by_birth_grace() -> Result<()> {
    let root_dir = tempfile::tempdir()?;
    let root = root_dir.path().to_str().context("root dir")?;

    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_BIRTH_GRACE_SECS", "1");
        cmd.env("CATENARY_ROOTS", root);
    })?;
    bridge.initialize()?;

    let pid = bridge
        .daemon_pid()
        .context("bridged daemon must publish its pid in the snapshot")?;

    // Hold the session well past the 1 s window — the connected bridge is
    // service, so the daemon must not exit underneath it.
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        anyhow::ensure!(
            pid_alive(pid),
            "the daemon (pid {pid}) must not exit under a connected bridge",
        );
        std::thread::sleep(POLL_SPACING);
    }

    // Ordinary clean shutdown still works; the daemon's teardown from here
    // is the landed disconnect-grace lifecycle (owned by the bridge's
    // DaemonGuard for this test).
    bridge.shutdown_clean(Duration::from_secs(10))?;
    Ok(())
}
