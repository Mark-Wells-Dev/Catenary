// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end coverage for the client-side command-filter fallback in the
//! `PreToolUse` hook (`check_shell_command`).
//!
//! `check_shell_command` (`src/cli/hooks.rs`) is the daemon-down fallback: when
//! the session-side IPC check is unreachable, it loads the user config and gates
//! the command against the configured allowlist client-side. Its logic is
//! unit-tested in-process via the extracted `check_resolved_command`, but the
//! `Config::load()` wrapper itself cannot be reached in-process (env reads, and
//! Rust 2024's `std::env::set_var` is `unsafe`, which this crate `forbid`s).
//!
//! These tests drive the real `catenary hook pre-tool` binary as a subprocess —
//! isolated env, an active allowlist via `CATENARY_CONFIG`, and NO daemon
//! running — so the client-side fallback is the path taken and the wrapper is
//! exercised end-to-end. A `-> None` mutant on `check_shell_command`
//! (allow-everything client-side) would make the denied command pass, failing
//! the deny assertion below.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{isolate_env, xdg_config_home};

/// Parse a Claude `PreToolUse` hook stdout into its `permissionDecision`, or
/// `None` when it is not a deny envelope (an allow is silent → empty stdout).
fn parse_decision(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let out = v.get("hookSpecificOutput")?;
    Some(out.get("permissionDecision")?.as_str()?.to_string())
}

/// Write a user config with an active allowlist (`allow = ["git"]`) under the
/// isolated `XDG_CONFIG_HOME`, then drive `catenary hook pre-tool --format=claude`
/// for `command` with NO daemon running, returning the hook's stdout.
fn run_client_hook(root: &str, command: &str) -> Result<String> {
    let config_dir = xdg_config_home(root).join("catenary");
    std::fs::create_dir_all(&config_dir).context("create config dir")?;
    std::fs::write(
        config_dir.join("config.toml"),
        "[commands]\nallow = [\"git\"]\n",
    )
    .context("write config")?;

    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.args(["hook", "pre-tool", "--format=claude"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn `hook pre-tool`")?;
    {
        let mut stdin = child.stdin.take().context("hook stdin")?;
        writeln!(stdin, "{payload}").context("write hook payload")?;
    }
    let out = child.wait_with_output().context("wait for hook")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A command absent from the active allowlist is denied by the client-side
/// fallback when no daemon is reachable. This is the only end-to-end exercise
/// of the `Config::load()` + delegate wrapper (`check_shell_command`); its
/// `-> None` mutant (allow everything) would let `cargo build` through.
#[test]
fn client_fallback_denies_non_allowlisted_command() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let stdout = run_client_hook(root, "cargo build")?;
    let decision = parse_decision(&stdout)
        .context("non-allowlisted command should produce a deny envelope")?;
    assert_eq!(
        decision, "deny",
        "client-side fallback must deny a non-allowlisted command, got: {stdout}"
    );
    Ok(())
}

/// Companion: an allowlisted command passes the client-side fallback (an allow
/// is silent — empty stdout, no deny envelope). Pins that the wrapper only
/// denies what it should, not everything.
#[test]
fn client_fallback_allows_allowlisted_command() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let stdout = run_client_hook(root, "git status")?;
    assert!(
        parse_decision(&stdout).is_none(),
        "client-side fallback must allow an allowlisted command (no deny), got: {stdout}"
    );
    Ok(())
}

/// misc 177: through the real hook binary, `--format=claude` declares a client
/// whose installed hook set registers `WorktreeCreate`, so agent-side
/// `catenary worktree add` is denied with the dispatch teaching. This pins the
/// one line of wiring the unit tests can't reach — `run_pre_tool` passing its
/// declared format into the matcher.
#[test]
fn worktree_add_denied_with_dispatch_teaching_for_claude_format() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let stdout = run_client_hook(root, "catenary worktree add topic")?;
    let decision = parse_decision(&stdout)
        .context("worktree add under --format=claude should produce a deny envelope")?;
    assert_eq!(
        decision, "deny",
        "worktree add must be denied for the claude format, got: {stdout}"
    );
    assert!(
        stdout.contains("WorktreeCreate") && stdout.contains("isolation:"),
        "the denial must teach the isolation dispatch flow, got: {stdout}"
    );
    Ok(())
}

/// Companion: the deny is surgical — the sanctioned cleanup verb `worktree rm`
/// stays allowed (silent stdout) under the same declared client.
#[test]
fn worktree_rm_still_allowed_for_claude_format() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let stdout = run_client_hook(root, "catenary worktree rm /tmp/wt")?;
    assert!(
        parse_decision(&stdout).is_none(),
        "worktree rm must stay allowed for the claude format (no deny), got: {stdout}"
    );
    Ok(())
}

/// Bug 80, leg 3 (confirm-intentional): the command filter kept enforcing
/// correctly with **no daemon** during the outage — a killed daemon must not
/// make the shell surface fail open *or* closed. This drives the real
/// `catenary start` → `catenary stop` lifecycle so the daemon was genuinely up
/// then died (the bug-80 sighting), then exercises the client-side fallback: a
/// non-allowlisted command is still denied, and an allowlisted one still passes.
#[test]
fn command_filter_enforces_after_daemon_death() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    // Bring a daemon up, then stop it — the daemon was up and died.
    let start = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, root);
        cmd.arg("start");
        cmd.output().context("run catenary start")?
    };
    assert!(
        start.status.success(),
        "catenary start must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&start.stderr),
    );
    let stop = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, root);
        cmd.args(["stop", "--force"]);
        cmd.output().context("run catenary stop")?
    };
    assert!(
        stop.status.success(),
        "catenary stop must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&stop.stderr),
    );

    // Never fail-open: a non-allowlisted command is still denied daemon-less.
    let denied = run_client_hook(root, "cargo build")?;
    let decision = parse_decision(&denied)
        .context("non-allowlisted command should deny even after daemon death")?;
    assert_eq!(
        decision, "deny",
        "filter must still deny daemon-less after a daemon death, got: {denied}",
    );

    // Never fail-closed: an allowlisted command still passes daemon-less.
    let allowed = run_client_hook(root, "git status")?;
    assert!(
        parse_decision(&allowed).is_none(),
        "filter must still allow daemon-less after a daemon death, got: {allowed}",
    );
    Ok(())
}
