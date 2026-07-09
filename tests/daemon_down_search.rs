// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Golden coverage for daemon-down `grep`/`glob` degradation (bug 80, leg 4).
//!
//! The maintainer ruling: degrade *honestly*, same machinery. Catenary is one
//! binary; the daemon's search pipeline is library code, so a daemon-less CLI
//! constructs the same engine in-process with no LSP manager. These tests pin
//! that the daemon-less output is **byte-identical on stdout** to a
//! daemon-served answer over the same tree with no language-server coverage —
//! and that the honesty marker rides on **stderr only** in daemon-less mode,
//! never in daemon-served mode, so `unenriched-because-uncovered` and
//! `unenriched-because-no-daemon` are never indistinguishable.
//!
//! Neither mode configures an LSP server: the daemon-served tree is uncovered,
//! which is exactly the render the daemon-less path reproduces.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use common::{BridgeProcess, isolate_env, xdg_state_home};

/// The mandatory daemon-less honesty marker (stderr only).
const NO_DAEMON_MARKER: &str =
    "[no daemon \u{2014} results unenriched; start one with catenary start]";

/// Populates a workspace tree with source files and a couple of hits. No LSP
/// server is ever configured, so every hit is uncovered in *both* modes.
fn populate_tree(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(root.join("src/alpha.rs"), "fn alpha() { needle(); }\n")?;
    std::fs::write(root.join("src/beta.rs"), "fn beta() { let x = needle; }\n")?;
    std::fs::write(root.join("src/notes.txt"), "no needle here at all\n")?;
    std::fs::write(root.join("README.md"), "# needle in the readme\n")?;
    Ok(())
}

/// Runs the `catenary` binary for `subargs` with cwd = `root`, isolated to
/// `state_home`, and returns `(stdout, stderr, success)`.
fn run_cli(state_home: &str, root: &Path, subargs: &[&str]) -> Result<(String, String, bool)> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(root)
        .args(subargs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().context("run catenary binary")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    ))
}

/// Runs `catenary <subargs>` against a *live daemon* over an **uncovered tree**
/// and returns `(stdout, stderr, success)`.
///
/// The tree is deliberately **not** registered as a workspace root: an uncovered
/// tree is one the daemon has no coverage for — no root, no language server — so
/// the daemon labels every hit as unenriched, exactly the state the daemon-less
/// path reproduces. Registering the root would make the comparison an
/// apples-to-oranges "covered-adjacent" tree and defeat the golden.
fn run_daemon_served(root: &Path, subargs: &[&str]) -> Result<(String, String, bool)> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();

    // A daemon with no roots and no LSP servers — the tree is fully uncovered.
    let mut bridge = BridgeProcess::spawn_in_state(&state_home, |_cmd| {})?;
    bridge.initialize()?;

    // Wait for the IPC socket the search binary connects to.
    let ipc_sock = xdg_state_home(state_dir.path())
        .join("catenary")
        .join("catenary.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ipc_sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ipc_sock.exists(), "daemon IPC socket should appear");

    let result = run_cli(&state_home, root, subargs)?;
    drop(bridge);
    Ok(result)
}

/// Runs `catenary <subargs>` with NO daemon (fresh, empty state dir) — the
/// in-process daemon-less path — and returns `(stdout, stderr, success)`.
fn run_daemon_less(root: &Path, subargs: &[&str]) -> Result<(String, String, bool)> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    run_cli(state_home, root, subargs)
}

/// The golden assertion for one `subargs` invocation: daemon-served stdout ==
/// daemon-less stdout (byte-identical), the marker rides stderr in daemon-less
/// mode only, and both exit 0.
fn assert_byte_identical(subargs: &[&str]) -> Result<()> {
    let root = tempfile::tempdir()?;
    populate_tree(root.path())?;

    let (served_out, served_err, served_ok) = run_daemon_served(root.path(), subargs)?;
    let (less_out, less_err, less_ok) = run_daemon_less(root.path(), subargs)?;

    assert!(served_ok, "daemon-served run must exit 0 for {subargs:?}");
    assert!(less_ok, "daemon-less run must exit 0 for {subargs:?}");

    assert_eq!(
        served_out, less_out,
        "daemon-served and daemon-less stdout must be byte-identical for {subargs:?}\n\
         --- daemon-served ---\n{served_out}\n--- daemon-less ---\n{less_out}",
    );

    assert!(
        !served_err.contains(NO_DAEMON_MARKER),
        "daemon-served stderr must NOT carry the no-daemon marker for {subargs:?}, got:\n{served_err}",
    );
    assert!(
        less_err.contains(NO_DAEMON_MARKER),
        "daemon-less stderr MUST carry the no-daemon marker for {subargs:?}, got:\n{less_err}",
    );
    Ok(())
}

#[test]
fn grep_daemon_less_stdout_is_byte_identical_to_daemon_served() -> Result<()> {
    assert_byte_identical(&["grep", "needle"])
}

#[test]
fn grep_daemon_less_count_is_byte_identical() -> Result<()> {
    assert_byte_identical(&["grep", "needle", "--count"])
}

#[test]
fn grep_daemon_less_no_match_is_byte_identical() -> Result<()> {
    assert_byte_identical(&["grep", "haystack_no_such_token"])
}

#[test]
fn glob_daemon_less_stdout_is_byte_identical_to_daemon_served() -> Result<()> {
    assert_byte_identical(&["glob", "src"])
}

#[test]
fn glob_daemon_less_pattern_is_byte_identical() -> Result<()> {
    assert_byte_identical(&["glob", "src/**/*.rs"])
}

#[test]
fn glob_daemon_less_count_is_byte_identical() -> Result<()> {
    assert_byte_identical(&["glob", "src", "--count"])
}

/// A daemon-less run still exits 0 (never a non-zero exit that would cancel a
/// sibling tool call in a parallel batch) and prints results on stdout — the
/// honest-degradation contract, not a hard failure.
#[test]
fn daemon_less_grep_exits_zero_with_results() -> Result<()> {
    let root = tempfile::tempdir()?;
    populate_tree(root.path())?;
    let (stdout, stderr, ok) = run_daemon_less(root.path(), &["grep", "needle"])?;
    assert!(ok, "daemon-less grep must exit 0");
    assert!(
        stdout.contains("needle"),
        "daemon-less grep must print real results on stdout, got:\n{stdout}"
    );
    assert!(
        stderr.contains(NO_DAEMON_MARKER),
        "daemon-less grep must print the marker on stderr, got:\n{stderr}"
    );
    Ok(())
}
