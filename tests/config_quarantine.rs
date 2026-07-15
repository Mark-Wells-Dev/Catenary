// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! End-to-end coverage for section-scoped config quarantine (bug 110).
//!
//! The incident: a `[commands]` section with two cross-reference validation
//! errors took down every surface — daemon-less `grep`/`glob` refused to run
//! (they never consume `[commands]`), the daemon refused to boot, and the
//! `PreToolUse` hook failed OPEN SILENTLY. The exact inversion of what should
//! happen. Quarantine restores the correct polarity, and these four proofs pin
//! it:
//!
//! 1. **grep degrades** — daemon-less `grep` runs (exit 0, correct results) on a
//!    config with a quarantined `[commands]`, printing ONE stderr advisory.
//! 2. **fail-open warns** — the `PreToolUse` hook, on a quarantined `[commands]`,
//!    ALLOWS a normally-denied command (enforcement off) and tells the agent so
//!    via `additionalContext`.
//! 3. **fail-closed opt-in** — with `client_enforcement_only = true` in the
//!    broken section, the SAME command is DENIED.
//! 4. **boot notifies once** — the daemon boots on a section-invalid config and
//!    fires exactly ONE desktop-notification intent (recorded under
//!    `CATENARY_NOTIFY_LOG`, the bug-111 seam).

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(clippy::panic, reason = "tests use panic for diagnostics")]

mod common;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, isolate_env, xdg_config_home, xdg_state_home};

/// The EXACT incident config shape: a `[commands]` with two cross-reference
/// errors — `deny.sqlite3` and `deny_flags.cargo`, both referencing commands
/// absent from `allow`. `allow = ["git"]` keeps the allowlist active, so a
/// fixed config WOULD enforce (proving fail-open is a degrade, not a no-op).
const BROKEN_COMMANDS: &str = "\
[commands]
allow = [\"git\"]

[commands.deny]
sqlite3 = [\"-cmd\"]

[commands.deny_flags]
cargo = [\"--offline\"]
";

/// The same broken section plus the fail-closed opt-in. `client_enforcement_only
/// = true` alongside enforcement fields is itself a validation error, so the
/// section is quarantined — but the raw opt-in is still recoverable, flipping the
/// hook to fail-closed.
const BROKEN_COMMANDS_FAIL_CLOSED: &str = "\
[commands]
client_enforcement_only = true
allow = [\"git\"]

[commands.deny]
sqlite3 = [\"-cmd\"]
";

/// Write a user config the isolated `catenary` reads (`XDG_CONFIG_HOME`).
fn write_user_config(root: &str, contents: &str) -> Result<()> {
    let dir = xdg_config_home(root).join("catenary");
    std::fs::create_dir_all(&dir).context("create config dir")?;
    std::fs::write(dir.join("config.toml"), contents).context("write config")?;
    Ok(())
}

// ── Proof 1: grep degrades (never dies on a section it doesn't consume) ──

/// Runs `catenary grep` daemon-less under `root`, isolated, with cwd = `tree`.
/// Returns `(stdout, stderr, success)`.
fn run_grep(root: &str, tree: &Path, pattern: &str) -> Result<(String, String, bool)> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.current_dir(tree)
        .args(["grep", pattern])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().context("run catenary grep")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    ))
}

/// grep-degrades: a quarantined `[commands]` (a section grep never consumes)
/// must only DEGRADE grep — exit 0, correct results, and ONE stderr advisory
/// naming the quarantined section — never kill it.
#[test]
fn grep_degrades_on_quarantined_commands() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;
    write_user_config(root, BROKEN_COMMANDS)?;

    let tree = tempfile::tempdir()?;
    std::fs::create_dir_all(tree.path().join("src")).context("mk src")?;
    std::fs::write(tree.path().join("src/a.rs"), "fn a() { needle(); }\n")
        .context("write source")?;

    let (stdout, stderr, ok) = run_grep(root, tree.path(), "needle")?;

    // Exit 0, real results on stdout — the search ran on the valid remainder.
    assert!(
        ok,
        "grep must exit 0 with a quarantined [commands], stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("needle"),
        "grep must print real results on stdout, got:\n{stdout}"
    );

    // Exactly one stderr advisory naming the quarantined section, once.
    let quarantine_lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("[commands]") && l.contains("quarantined"))
        .collect();
    assert_eq!(
        quarantine_lines.len(),
        1,
        "grep must print the quarantine advisory exactly once, got stderr:\n{stderr}"
    );
    assert!(
        quarantine_lines[0].contains("catenary doctor"),
        "the advisory must point at the fix: {}",
        quarantine_lines[0]
    );
    Ok(())
}

// ── Proofs 2 & 3: the hook degrades LOUDLY (fail-open warns / fail-closed) ──

/// Parse a Claude `PreToolUse` hook stdout into its `permissionDecision`, or
/// `None` when it is not a deny envelope.
fn parse_decision(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    v.get("hookSpecificOutput")?
        .get("permissionDecision")?
        .as_str()
        .map(str::to_string)
}

/// Parse a Claude `PreToolUse` hook stdout into its `additionalContext`, or
/// `None` when the envelope carries none.
fn parse_additional_context(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    v.get("hookSpecificOutput")?
        .get("additionalContext")?
        .as_str()
        .map(str::to_string)
}

/// Drive `catenary hook pre-tool --format=claude` for `command` under `root`
/// (NO daemon running), with `CATENARY_NOTIFY_LOG` at `notify_log`. Returns the
/// hook's stdout.
fn run_hook(root: &str, notify_log: &Path, command: &str) -> Result<String> {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.env("CATENARY_NOTIFY_LOG", notify_log);
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

/// Count the lines in the notify tally (each line is one fired notification).
fn notify_count(notify_log: &Path) -> usize {
    std::fs::read_to_string(notify_log)
        .map_or(0, |s| s.lines().filter(|l| !l.trim().is_empty()).count())
}

/// fail-open-warns: on a quarantined `[commands]`, the hook treats the section
/// as absent (enforcement OFF) — so `cargo build`, a command a FIXED config
/// would deny, is ALLOWED — and the agent is told enforcement is off via
/// `additionalContext`. One desktop-notification intent fires (the loud channel).
#[test]
fn hook_fail_open_allows_and_warns() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;
    write_user_config(root, BROKEN_COMMANDS)?;
    let notify_log = dir.path().join("notify_tally.log");

    // `cargo` is not in `allow = ["git"]` — a fixed config would deny it. With
    // [commands] quarantined, enforcement is off, so it is allowed (no deny).
    let stdout = run_hook(root, &notify_log, "cargo build")?;

    assert!(
        parse_decision(&stdout).is_none(),
        "fail-open must ALLOW a normally-denied command (no deny), got:\n{stdout}"
    );
    let ctx = parse_additional_context(&stdout)
        .context("fail-open must tell the agent enforcement is off via additionalContext")?;
    assert!(
        ctx.contains("[commands] quarantined") && ctx.contains("OFF"),
        "the additionalContext must name the quarantine and say filtering is OFF: {ctx}"
    );

    // The loud channel fired once (the onset).
    assert_eq!(
        notify_count(&notify_log),
        1,
        "the fail-open onset must fire exactly one desktop-notification intent",
    );
    Ok(())
}

/// fail-closed opt-in: with `client_enforcement_only = true` in the broken
/// section (recovered best-effort from the invalid `[commands]`), the SAME
/// normally-denied command is DENIED with a teaching message.
#[test]
fn hook_fail_closed_denies_with_opt_in() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;
    write_user_config(root, BROKEN_COMMANDS_FAIL_CLOSED)?;
    let notify_log = dir.path().join("notify_tally.log");

    let stdout = run_hook(root, &notify_log, "cargo build")?;

    let decision =
        parse_decision(&stdout).context("fail-closed opt-in must produce a deny envelope")?;
    assert_eq!(
        decision, "deny",
        "with client_enforcement_only=true the broken section must fail CLOSED, got:\n{stdout}"
    );
    let v: Value = serde_json::from_str(stdout.trim()).context("hook stdout is JSON")?;
    let reason = v
        .pointer("/hookSpecificOutput/permissionDecisionReason")
        .and_then(Value::as_str)
        .context("deny reason present")?;
    assert!(
        reason.contains("client_enforcement_only") || reason.contains("DENIED"),
        "the deny must teach why (the config error / opt-in): {reason}"
    );
    Ok(())
}

/// Companion to proof 3: a catenary-own command is NOT denied even under the
/// fail-closed opt-in — the spec denies *non-catenary* commands, and catenary's
/// navigation tools must keep working while the config is broken.
#[test]
fn hook_fail_closed_still_allows_catenary_command() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;
    write_user_config(root, BROKEN_COMMANDS_FAIL_CLOSED)?;
    let notify_log = dir.path().join("notify_tally.log");

    let stdout = run_hook(root, &notify_log, "catenary grep needle")?;
    assert!(
        parse_decision(&stdout).as_deref() != Some("deny"),
        "a catenary command must never be denied by the fail-closed quarantine, got:\n{stdout}"
    );
    Ok(())
}

// ── Proof 4: the daemon boots on the valid remainder, notifies once ──

/// The IPC socket path under an isolated state home.
fn ipc_socket(root: &str) -> std::path::PathBuf {
    xdg_state_home(root).join("catenary").join("catenary.sock")
}

/// boot-with-quarantine-notifies-once: the daemon boots on a section-invalid
/// config (the MCP `initialize` handshake succeeds, proving it came fully up past
/// config load) and fires exactly ONE desktop-notification intent naming the
/// quarantined section.
#[test]
fn daemon_boots_and_notifies_once_on_quarantine() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    // The config the DAEMON reads is a named file (CATENARY_CONFIG), written into
    // the isolated tree so the daemon subprocess (spawned by the bridge with the
    // inherited env) picks it up.
    let config_path = dir.path().join("daemon-config.toml");
    std::fs::write(&config_path, BROKEN_COMMANDS).context("write daemon config")?;
    let notify_log = dir.path().join("notify_tally.log");

    // `config_path` is not needed again after the write above; `notify_log` is
    // (the tally is read below), so only it is cloned into the closure.
    let notify_env = notify_log.clone();
    let mut bridge = BridgeProcess::spawn_in_state(root, move |cmd| {
        cmd.env("CATENARY_CONFIG", &config_path);
        cmd.env("CATENARY_NOTIFY_LOG", &notify_env);
    })?;

    // A successful `initialize` proves the daemon booted fully — past the
    // Config::load() where the boot-quarantine notification fires — rather than
    // refusing outright the way the incident daemon did.
    bridge.initialize()?;

    // The IPC socket the daemon bound must be present (a booted daemon).
    let sock = ipc_socket(root);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        sock.exists(),
        "the daemon must have booted and bound its IPC socket"
    );

    // Exactly one notification intent fired, naming the quarantine.
    let tally = std::fs::read_to_string(&notify_log).unwrap_or_default();
    let quarantine_lines: Vec<&str> = tally
        .lines()
        .filter(|l| l.contains("quarantined") || l.to_lowercase().contains("quarantine"))
        .collect();
    assert_eq!(
        quarantine_lines.len(),
        1,
        "the daemon must fire exactly ONE boot-quarantine notification, got tally:\n{tally}"
    );
    assert!(
        quarantine_lines[0].contains("[commands]"),
        "the notification must name the quarantined section: {}",
        quarantine_lines[0]
    );

    drop(bridge);
    Ok(())
}
