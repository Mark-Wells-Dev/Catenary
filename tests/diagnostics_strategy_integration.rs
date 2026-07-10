// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the diagnostics pipeline.
//!
//! Uses mockls with various flags to exercise pipeline behavior:
//! - Default (settle + push cache)
//! - Version matching (`--publish-version`)
//! - Progress tokens (`--progress-on-change`)
//! - Pull-only (`--pull-diagnostics --no-push-diagnostics`)
//! - Server death (`--drop-after`)

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, ipc_request, ipc_request_progress_aware, read_merged_log};

const MOCK_LANG_A: &str = "yX4Za";

/// Spawns a bridge with mockls configured for `MOCK_LANG_A`.
///
/// Wraps [`common::BridgeProcess::spawn`] to accept mockls flags
/// instead of fully-formed `CATENARY_SERVERS` specs.
fn spawn_mockls(mockls_args: &[&str], root: &str) -> Result<BridgeProcess> {
    let flags = mockls_args.join(" ");
    let lsp = common::mockls_lsp_arg(MOCK_LANG_A, &flags);
    BridgeProcess::spawn(&[&lsp], root)
}

/// Counts request-log lines whose `method` equals `method` (misc 153).
///
/// mockls's `--request-log` appends one `{"method":"..."}` object per handled
/// request. `call_diagnostics` runs the whole pipeline to completion before it
/// returns, so every pull the retrieval decided to issue is already logged —
/// the count is authoritative with no sleep or poll.
fn count_request_method(log: &str, method: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v.get("method").and_then(Value::as_str) == Some(method))
        .count()
}

/// Default mockls: publishes diagnostics on didOpen/didChange without
/// version or progress tokens. With settle-based pipeline, diagnostics
/// are retrieved after the server process tree goes quiet — no strategy
/// discovery needed.
#[test]
fn test_diagnostics_default_mockls() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(&[], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Default mockls should return diagnostics via settle + push cache. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version`: includes version field in
/// publishDiagnostics. Exercises the Version strategy.
#[test]
fn test_diagnostics_version_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(&["--publish-version"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Version path should return diagnostics. Got: {text}"
    );

    Ok(())
}

/// mockls with `--progress-on-change`: sends progress tokens around
/// diagnostic computation on `didChange`. Exercises the `TokenMonitor` strategy.
///
/// Progress tokens are only sent on `didChange` (not `didOpen`), so
/// the first call opens the file (degraded mode), and the second call
/// after modification triggers the progress path.
#[test]
fn test_diagnostics_token_monitor_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--progress-on-change"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First call: opens the file via didOpen (no progress tokens sent)
    let _ = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // Modify file to trigger didChange on next call
    std::fs::write(&file, "echo changed\necho line3\n")?;

    // Second call: triggers didChange → progress tokens → TokenMonitor
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "TokenMonitor path should return diagnostics on didChange. Got: {text}"
    );

    Ok(())
}

/// mockls with `--drop-after 3`: crashes after 3 responses (initialize,
/// eager health probe, first diagnostics request). Verifies
/// `ServerDied` is handled during the diagnostics pipeline.
#[test]
fn test_diagnostics_server_death() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(&["--drop-after", "3"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    // Server survives the eager health probe (response #2) but dies
    // during diagnostics processing (response #3).
    let text = bridge
        .call_diagnostics(file.to_str().context("path")?)
        .unwrap_or_default();

    // Should either get diagnostics (if server published before dying), a status
    // message, a notify error, or an `[unverified — <server> returned no result]`
    // line (bug 56): a server that dies before retrieval produces no result, so
    // the file is neither `[clean]` nor dirty, but it is still surfaced as an
    // explicit unverified line rather than vanishing into empty stdout. An empty
    // string is still tolerated for the degenerate case where the CLI call itself
    // faults (IPC error → `unwrap_or_default`). No raw infrastructure messages to
    // the agent.
    let is_acceptable = text.contains("mock diagnostic")
        || text.contains("unverified")
        || text.contains("[no language server]")
        || text.trim().is_empty()
        || text.contains("Notify error");

    assert!(
        is_acceptable,
        "Server death should be handled gracefully. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version --no-code-actions`: server does not
/// advertise `codeActionProvider`. Diagnostics should appear without
/// any `fix:` lines (the capability gate in `process_file_inner` skips
/// code action requests entirely).
#[test]
fn test_diagnostics_no_code_actions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--publish-version", "--no-code-actions"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Should contain diagnostics. Got: {text}"
    );
    assert!(
        !text.contains("fix:"),
        "Should NOT contain fix: lines when code actions are disabled. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version --multi-fix`: server returns multiple
/// quickfix actions per diagnostic. Each diagnostic should have two
/// `fix:` lines (the primary and the alternative).
#[test]
fn test_diagnostics_multi_fix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--publish-version", "--multi-fix"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Should contain diagnostics. Got: {text}"
    );

    let fix_count = text.lines().filter(|l| l.contains("fix:")).count();
    assert!(
        fix_count >= 2,
        "Multi-fix mode should produce at least 2 fix: lines. Got {fix_count} in: {text}"
    );
    assert!(
        text.contains("fix: alternative for"),
        "Should contain alternative fix. Got: {text}"
    );

    Ok(())
}

/// Default mockls with `--publish-version` now always includes a
/// `refactor` code action alongside quickfix actions. Verify that
/// refactor actions are filtered out and only `fix:` lines from
/// quickfix actions appear in the output.
#[test]
fn test_diagnostics_refactor_filtered() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(&["--publish-version"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("fix:"),
        "Should contain quickfix fix: lines. Got: {text}"
    );
    assert!(
        !text.contains("refactor"),
        "Refactor actions should be filtered out. Got: {text}"
    );

    Ok(())
}

/// mockls with `--pull-diagnostics --no-push-diagnostics`: server advertises
/// pull diagnostics but never pushes. Verifies that Catenary uses the pull
/// path to retrieve diagnostics instead of returning `[diagnostics unavailable]`.
#[test]
fn test_diagnostics_pull_only() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--pull-diagnostics", "--no-push-diagnostics"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Pull-only server should return diagnostics via pull path. Got: {text}"
    );

    Ok(())
}

/// Verifies that quick-fix code actions from the LSP server appear as
/// `fix:` lines in the hook diagnostics output.
///
/// mockls advertises `codeActionProvider: true` and returns quickfix
/// code actions for diagnostics with source "mockls".
#[test]
fn test_diagnostics_code_action_enrichment() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(&["--publish-version"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // mockls publishes diagnostics with source "mockls" and returns
    // quickfix code actions with title "fix: <message>" for those.
    assert!(
        text.contains("mock diagnostic"),
        "Should contain diagnostics. Got: {text}"
    );
    assert!(
        text.contains("fix:"),
        "Should contain fix: lines from code actions. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version --advertise-save --flycheck-command mockc`:
/// Exercises the multi-round diagnostics pattern (Gap 1). After `didSave`,
/// mockls spawns mockc as a subprocess under a `$/progress` bracket. Native
/// diagnostics arrive immediately; flycheck diagnostics arrive after mockc
/// finishes. Catenary should wait for the full Active→Idle progress cycle,
/// returning flycheck diagnostics (which contain "flycheck") rather than
/// short-circuiting on the first native diagnostics.
#[test]
fn test_diagnostics_flycheck_multi_round() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mut bridge = spawn_mockls(
        &[
            "--publish-version",
            "--advertise-save",
            "--flycheck-command",
            mockc_bin,
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First call: opens the file (native diagnostics only, no flycheck)
    let _ = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // Modify file to trigger didChange + didSave on next call
    std::fs::write(&file, "echo changed\necho line3\n")?;

    // Second call: triggers didChange + didSave → flycheck subprocess
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // Should contain diagnostics reflecting the modified file (2 lines).
    // The flycheck subprocess runs under a progress bracket; Catenary must
    // wait for the full Active→Idle cycle to get the post-flycheck diagnostics.
    assert!(
        text.contains("mock diagnostic") && text.contains("2 lines"),
        "Multi-round path should return flycheck diagnostics for \
         the modified file (2 lines). Got: {text}"
    );

    Ok(())
}

/// Bug 28: the settle must hold while a flycheck child burns CPU **without** an
/// open `$/progress` bracket, then report the diagnostic the child publishes.
///
/// mockls runs the flycheck subprocess (mockc) with `--flycheck-no-progress`, so
/// the lifecycle stays `Healthy` for the child's whole run. The idle detector
/// watches the full subtree, so cargo/rustc-style child CPU keeps it from
/// settling until the child exits and publishes. This regressed when the settle
/// grew a tree-summed CPU budget that bailed (`BudgetExhausted`, treated as a
/// successful settle) on exactly this legitimate child CPU and returned
/// `[clean]`; failure detection no longer bounds the settle on tree CPU.
#[test]
fn test_diagnostics_settle_holds_through_unbracketed_child_flycheck() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mut bridge = spawn_mockls(
        &[
            "--advertise-save",
            "--diagnostics-on-save",
            "--flycheck-command",
            mockc_bin,
            "--flycheck-no-progress",
            "--flycheck-ticks",
            "120",
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "settle must hold while the flycheck child burns CPU and report its \
         diagnostic, not settle over the busy child and return [clean]. Got: {text}"
    );

    Ok(())
}

/// mockls with `--progress-on-change --no-push-diagnostics`: server sends
/// progress tokens but never publishes diagnostics. After settle, the push
/// cache is empty and pull is not supported → the file is verified clean and
/// listed explicitly as `[clean]` (ws37 ticket 01, retiring silent-on-clean).
#[test]
fn test_diagnostics_no_push_no_pull_returns_clean() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--progress-on-change", "--no-push-diagnostics"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("[clean]") && text.contains("test."),
        "Server with no push and no pull should list the file as `[clean]`. Got: {text}"
    );

    Ok(())
}

/// Near-threshold flycheck: mockc burns 900 ticks (~9s wall time) under
/// a `$/progress` bracket. mockls is Sleeping while the subprocess runs,
/// so the threshold does not drain (subprocess ticks don't count against
/// mockls). After mockc finishes, mockls publishes diagnostics with a
/// version match.
#[test]
fn test_near_threshold_flycheck() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mut bridge = spawn_mockls(
        &[
            "--publish-version",
            "--advertise-save",
            "--flycheck-command",
            mockc_bin,
            "--flycheck-ticks",
            "900",
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First call opens the file and gets initial diagnostics
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Initial diagnostics should arrive. Got: {text}"
    );

    // Modify the file to trigger flycheck on the second call
    std::fs::write(&file, "echo changed\necho line3\n")?;

    // Second call: triggers didChange + didSave → flycheck with 900-tick mockc
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Near-threshold flycheck should return diagnostics (mockls sleeps \
         while mockc runs, threshold not drained). Got: {text}"
    );

    Ok(())
}

/// Sends a signal to `pid` via the `kill(1)` utility.
///
/// The daemon is a detached grandchild with no `Child` handle to reach, and the
/// workspace-wide `forbid(unsafe_code)` rules out a direct `libc::kill`, so the
/// safe path is the `kill` binary. Used only by the wedged-daemon regression.
#[cfg(unix)]
fn signal_process(pid: u32, signal: &str) -> Result<()> {
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .context("spawn kill")?;
    anyhow::ensure!(status.success(), "kill -{signal} {pid} exited {status}");
    Ok(())
}

/// A wedged daemon — SIGSTOP'd mid-request — must fail the progress-aware
/// `tool/editing-stop` wait within its no-progress budget, not hang for minutes
/// on a wall clock (misc 130 / bug 59). While the daemon is frozen it shows no
/// progress (flat `utime`/`stime`/`pfc`/context-switch counters, stopped
/// scheduler state), so the wait charges its budget across consecutive dead
/// windows and bails; a saturated-but-*working* daemon would keep advancing its
/// counters every window and never trip it.
///
/// Drives the identical mechanism `ipc_request_long` uses, via
/// `ipc_request_progress_aware` on a short budget, so the regression is fast and
/// deterministic — it need not sit out the production 45-second budget to prove
/// the fail-fast path.
#[cfg(unix)]
#[test]
fn wedged_daemon_fails_within_no_progress_budget() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;
    let file_str = file.to_str().context("path")?;

    let mut bridge = spawn_mockls(&[], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;
    let pid = bridge.daemon_pid().context("daemon PID from state.json")?;

    // Stage the editing handoff while the daemon is still alive — the short
    // `ipc_request` calls `call_diagnostics` makes before the long wait.
    ipc_request(
        &socket,
        &json!({"method": "pre-tool/editing-start", "agent_id": ""}),
    )?;
    ipc_request(
        &socket,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": file_str,
            "agent_id": ""
        }),
    )?;
    ipc_request(
        &socket,
        &json!({"method": "pre-tool/editing-stop", "agent_id": ""}),
    )?;

    // Freeze the daemon: it can no longer make progress on the pipeline.
    signal_process(pid, "STOP")?;

    // The progress-aware wait must give up within the (short) no-progress budget.
    let budget = Duration::from_secs(3);
    let started = std::time::Instant::now();
    let result = ipc_request_progress_aware(
        &socket,
        Some(pid),
        &json!({"method": "tool/editing-stop"}),
        budget,
    );
    let elapsed = started.elapsed();

    // Resume and reap the frozen daemon before asserting, so a failing assert
    // never leaves a stopped process behind.
    let _ = signal_process(pid, "CONT");
    let _ = signal_process(pid, "KILL");

    assert!(
        result.is_err(),
        "a wedged (SIGSTOP) daemon must fail the wait, not return a response: {result:?}"
    );
    assert!(
        elapsed < budget + Duration::from_secs(30),
        "wedged wait must fail fast within its no-progress budget (~{budget:?}), \
         took {elapsed:?}"
    );

    Ok(())
}

/// mockls with `--pull-diagnostics --fail-pull --no-push-diagnostics`:
/// pull fails on first call → downgrade to push-only → clean. Second call
/// skips pull (downgraded) → clean. A verified-clean file is listed
/// explicitly as `[clean]` (ws37 ticket 01, retiring silent-on-clean).
#[test]
fn test_pull_downgrade_no_push() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--pull-diagnostics", "--fail-pull", "--no-push-diagnostics"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First call: pull fails → downgrade → clean → `[clean]`
    let text1 = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text1.contains("[clean]"),
        "Failed pull with no push should list the file as `[clean]`. Got: {text1}"
    );

    // Second call: pull skipped (downgraded) → clean → `[clean]`
    let text2 = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text2.contains("[clean]"),
        "Downgraded server should list the file as `[clean]` without retrying pull. Got: {text2}"
    );

    Ok(())
}

/// mockls with `--pull-diagnostics --fail-pull --publish-version`:
/// push is working, pull fails → downgrade → push cache has data →
/// returns diagnostics.
#[test]
fn test_pull_downgrade_with_push() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--pull-diagnostics", "--fail-pull", "--publish-version"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // Push cache is populated (push works), pull fails but push data
    // is returned before pull is attempted.
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Server with working push should return diagnostics even with broken pull. Got: {text}"
    );

    Ok(())
}

// ─── Push-received precedence (misc 153) ──────────────────────────────

/// Never-heard draws exactly one best-effort probe (misc 153 / bug 74).
///
/// A silent mockls — never publishes (`--no-push-diagnostics`) and does not
/// advertise `diagnosticProvider` — leaves the push cache empty (never-heard).
/// `retrieve_diagnostics` then issues exactly one best-effort
/// `textDocument/diagnostic`, the bug-74 rescue for a genuinely silent server.
/// mockls answers a pull it never advertised with an empty report, so the file
/// verifies `[clean]`, and the request log carries precisely one probe.
#[test]
fn never_heard_draws_one_best_effort_probe() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let rlog_arg = rlog.to_str().context("rlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--no-push-diagnostics", "--request-log", rlog_arg],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("[clean]"),
        "a silent server verifies clean via the best-effort probe. Got: {text}"
    );

    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "textDocument/diagnostic"),
        1,
        "a never-heard file draws exactly one best-effort pull; log:\n{rlog_text}"
    );

    Ok(())
}

/// A publish racing the best-effort pull must reach the receipt (bug 99).
///
/// Byte-exact model of the first live macOS conformance run's
/// lua-language-server incident (run 29067405830): a debouncing push-only
/// server publishes its diagnostics while the daemon's best-effort pull is
/// in flight, then rejects the pull with `-32601`. mockls
/// `--publish-then-reject-pull` writes exactly that wire order — publish
/// first, rejection second — so the sequential reader is GUARANTEED to have
/// cached the publish before the pull future resolves. Pre-fix, the pull's
/// empty result was final and the receipt falsified `[clean]` over evidence
/// in hand; the fix re-consults the push cache after an empty pull.
#[test]
fn publish_racing_the_pull_reaches_the_receipt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    // --no-push-diagnostics keeps the cache empty at retrieval (never-heard),
    // forcing the best-effort pull that the racing publish then overtakes.
    let mut bridge = spawn_mockls(
        &["--no-push-diagnostics", "--publish-then-reject-pull"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "the publish that raced the rejected pull is evidence in hand — the \
         receipt must carry it, not render [clean] (bug 99). Got: {text}"
    );
    assert!(
        !text.contains("[clean]"),
        "a receipt over a cached truthful publish must not be [clean]. Got: {text}"
    );

    Ok(())
}

/// A publish landing AFTER the rejected probe resolves must still reach the
/// receipt — the bug-99 residual, the yaml-language-server incident shape
/// (run 29068921605, WITH the bug-99 re-consult fix in place).
///
/// mockls models the victim exactly: publishes only on `didSave`
/// (`--diagnostics-on-save --advertise-save`), ~300 ms after it
/// (`--diagnostics-delay 300` — the silent debounce, no progress bracket, no
/// CPU: a sleeping timer thread the settle activity model cannot see), and
/// answers the best-effort probe with `-32601` (`--reject-pull` — no pull
/// support at all).
///
/// Call 1 is first contact: the server has never published on this
/// connection, so the retrieval evidence bar is unarmed and the receipt may
/// render the stated first-contact residual (not asserted). Its delayed
/// publish then lands, making the server demonstrably push. Call 2 is the
/// incident: never-heard at settle, but the bar is now armed — retrieval must
/// hold until the debounced publish arrives and the receipt must carry the
/// diagnostic, where pre-fix it falsified `[clean]`.
#[test]
fn delayed_publish_after_settle_reaches_the_receipt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let rlog_arg = rlog.to_str().context("rlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &[
            "--diagnostics-on-save",
            "--advertise-save",
            "--diagnostics-delay",
            "300",
            "--reject-pull",
            "--request-log",
            rlog_arg,
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First contact — seeds the push evidence. The delayed publish lands
    // ~300 ms after this call's didSave; give it time to be dispatched so
    // call 2's evidence bar is deterministically armed.
    let _first = bridge.call_diagnostics(file.to_str().context("path")?)?;
    std::thread::sleep(Duration::from_millis(600));

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "a demonstrated-push server's debounced publish must be waited for \
         and reach the receipt (bug 99 residual). Got: {text}"
    );
    assert!(
        !text.contains("[clean]"),
        "the receipt must not render [clean] over a pending publish. Got: {text}"
    );

    // The evidence bar heard the publish, so the second call never needed the
    // probe — at most first contact's one rejected probe appears in the log.
    let rlog_text = read_merged_log(&rlog);
    assert!(
        count_request_method(&rlog_text, "textDocument/diagnostic") <= 1,
        "once the evidence bar hears the publish, no probe fires; log:\n{rlog_text}"
    );

    Ok(())
}

/// When the evidence bar expires with no publish and the probe goes
/// unanswered, the file renders `[unverified — … returned no result]`,
/// never `[clean]` — the honest resolution of the bar's own residual.
///
/// mockls with `--publish-once --diagnostics-on-save` publishes on the first
/// `didSave` only — a push server that never re-publishes an unchanged
/// document (the bug-74 shape) — and `--reject-pull` answers the probe with
/// `-32601`, so on the second run there is NO evidence channel at all:
/// never-heard, no publish coming, probe rejected. The bare receipt's trust
/// contract (`[clean]` means evidenced-clean) demands the unverified line
/// here; the pre-bar pipeline rendered absence as `[clean]`. (Gating the one
/// publish on `didSave` keeps it out of the daemon's spawn-time eager health
/// probe, whose `didOpen`/`didClose` of the same file would otherwise consume
/// it before the batch's clear-then-open.)
#[test]
fn expired_evidence_renders_unverified_not_clean() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let rlog_arg = rlog.to_str().context("rlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &[
            "--publish-once",
            "--diagnostics-on-save",
            "--advertise-save",
            "--reject-pull",
            "--request-log",
            rlog_arg,
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // Call 1: the single publish fires on the batch's didSave — heard,
    // reported, and the server is now demonstrably push.
    let first = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        first.contains("mock diagnostic"),
        "first contact hears the one publish. Got: {first}"
    );

    // Call 2: never-heard, bar armed, no publish ever comes (publish-once),
    // probe rejected. The dead-air budget drains and the file must resolve
    // unverified — an honest no-verdict — not an absence-of-evidence [clean].
    let second = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        second.contains("[unverified") && second.contains("returned no result"),
        "an expired evidence bar with a rejected probe renders the unverified \
         line. Got: {second}"
    );
    assert!(
        !second.contains("[clean]"),
        "no evidence means no [clean] — the trust contract is absolute. Got: {second}"
    );
    assert!(
        !second.contains("stuck"),
        "the server is alive and merely silent — 'stuck' is a process-state \
         claim (misc 160) and must not appear. Got: {second}"
    );

    // The probe was still attempted (it could have been the evidence) —
    // exactly once, on the second call.
    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "textDocument/diagnostic"),
        1,
        "the expired bar still draws the one best-effort probe; log:\n{rlog_text}"
    );

    Ok(())
}

/// Heard-empty: an explicit empty publish is evidence — no probe (misc 153).
///
/// mockls with `--push-empty` publishes `"diagnostics": []` on `didOpen` — the
/// push-only Lattice-16 contract shape: a clean file gets an explicit empty
/// publish, not silence. The push cache then holds `Some(vec![])`
/// (heard-empty), which is authoritative: `retrieve_diagnostics` reports
/// `[clean]` backed by that evidence and never fires the best-effort probe.
/// The request log shows zero `textDocument/diagnostic`. This is the
/// end-to-end twin of `never_heard_draws_one_best_effort_probe`: same absent
/// capability, opposite publish behavior, opposite probe outcome.
#[test]
fn heard_empty_push_suppresses_probe() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let rlog_arg = rlog.to_str().context("rlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--push-empty", "--request-log", rlog_arg],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("[clean]"),
        "an explicit empty publish reads as clean. Got: {text}"
    );

    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "textDocument/diagnostic"),
        0,
        "heard-empty is evidence — the probe must not fire; log:\n{rlog_text}"
    );

    Ok(())
}

/// Heard-dirty: a push publish wins outright — no pull of any kind (misc 153).
///
/// Default mockls publishes a non-empty diagnostic on `didOpen`, so the push
/// cache holds evidence (`Some(non-empty)`). `retrieve_diagnostics` reports it
/// and never consults pull, so a push server is never double-reported and no
/// off-spec probe fires. The request log shows zero `textDocument/diagnostic`.
#[test]
fn heard_dirty_push_wins_no_pull() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let rlog_arg = rlog.to_str().context("rlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--request-log", rlog_arg],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "a heard push diagnostic should be reported. Got: {text}"
    );

    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "textDocument/diagnostic"),
        0,
        "a heard push must never trigger a pull; log:\n{rlog_text}"
    );

    Ok(())
}

// ─── Multi-server diagnostics ─────────────────────────────────────────

/// Two servers with diagnostics enabled: output contains diagnostics from
/// both (concatenation model). Each server independently settles, retrieves,
/// filters, and formats its own diagnostics.
#[test]
fn test_diagnostics_multi_server_concatenation() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(
            root.join(format!("test.{MOCK_LANG_A}")),
            "line one\nline two\n",
        )?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[lsp.server.mockls-a]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [lsp.server.mockls-b]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [lsp.language.{MOCK_LANG_A}]\n\
                 servers = [\"mockls-a\", \"mockls-b\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("test.{MOCK_LANG_A}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // Both servers publish "mock diagnostic" — the output should contain
    // the diagnostic text (at least once; both servers produce the same
    // diagnostic so we verify it appears).
    assert!(
        text.contains("mock diagnostic"),
        "Multi-server output should contain diagnostics. Got:\n{text}"
    );
    // The output should NOT be "[clean]" or "[no language server]"
    assert!(
        !text.contains("[clean]") && !text.contains("[no language server]"),
        "Expected diagnostics from both servers, got:\n{text}"
    );

    Ok(())
}

/// One server has `diagnostics = false` in its binding: only the other
/// server's diagnostics appear.
#[test]
fn test_diagnostics_one_server_suppressed() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("test.{MOCK_LANG_A}")), "echo hello\n")?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[lsp.server.mockls-diag]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [lsp.server.mockls-nodiag]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [lsp.language.{MOCK_LANG_A}]\n\
                 servers = [\"mockls-diag\", {{ name = \"mockls-nodiag\", diagnostics = false }}]\n"
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("test.{MOCK_LANG_A}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // Only one server contributes diagnostics
    assert!(
        text.contains("mock diagnostic"),
        "Diagnostic-enabled server should contribute. Got:\n{text}"
    );

    Ok(())
}

/// Server A has `min_severity = "error"` (filters warnings), server B has
/// no threshold. mockls publishes severity 2 (warning). Only server B's
/// diagnostics pass through.
#[test]
fn test_diagnostics_per_server_min_severity() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("test.{MOCK_LANG_A}")), "echo hello\n")?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[lsp.server.mockls-strict]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\
                 min_severity = \"error\"\n\n\
                 [lsp.server.mockls-lax]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [lsp.language.{MOCK_LANG_A}]\n\
                 servers = [\"mockls-strict\", \"mockls-lax\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("test.{MOCK_LANG_A}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // mockls emits severity 2 (warning). mockls-strict filters it out,
    // mockls-lax passes it through. We should see diagnostics from the
    // lax server.
    assert!(
        text.contains("mock diagnostic"),
        "Lax server's warnings should pass through. Got:\n{text}"
    );

    Ok(())
}

/// Language-level `diagnostics = false`: no servers contribute diagnostics,
/// file shown with `[no LSP coverage]`.
#[test]
fn test_diagnostics_no_servers() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("test.{MOCK_LANG_A}")), "echo hello\n")?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[lsp.server.mockls-only]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [lsp.language.{MOCK_LANG_A}]\n\
                 diagnostics = false\n\
                 servers = [\"mockls-only\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("test.{MOCK_LANG_A}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // All servers suppressed — file shown with [no LSP coverage]
    assert!(
        !text.contains("[LSP available]"),
        "No LSP available header. Got:\n{text}"
    );
    assert!(
        text.contains(&format!("test.{MOCK_LANG_A}")),
        "Suppressed file should appear in output. Got:\n{text}"
    );
    assert!(
        text.contains("[no LSP coverage]"),
        "Suppressed file should show no LSP coverage. Got:\n{text}"
    );

    Ok(())
}

/// One server dies during settle: the other server's diagnostics are
/// still collected (graceful degradation per §13).
#[test]
fn test_diagnostics_one_server_dies() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("test.{MOCK_LANG_A}")), "echo hello\n")?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[lsp.server.mockls-crash]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--drop-after\", \"3\"]\n\n\
                 [lsp.server.mockls-stable]\n\
                 path = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [lsp.language.{MOCK_LANG_A}]\n\
                 servers = [\"mockls-crash\", \"mockls-stable\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("test.{MOCK_LANG_A}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    // mockls-crash dies after 3 responses (initialize response +
    // initialized ack + didOpen). mockls-stable should still produce
    // diagnostics (or list the file `[clean]` if it verified no diagnostics).
    assert!(
        text.contains("mock diagnostic") || text.contains("[clean]"),
        "Surviving server should still contribute. Got:\n{text}"
    );
    // Should NOT be entirely "[no language server]"
    assert!(
        !text.contains("[no language server]"),
        "Surviving server should prevent [no language server]. Got:\n{text}"
    );

    Ok(())
}

/// In-run bounded recovery (decision 027, ticket 05). The server dies during
/// the batch's post-save settle on its FIRST life — before any diagnostic is
/// retrieved, so the file resolves `NoResults`. One bounded respawn (the
/// `--die-once-file` marker is now present, so the respawn runs healthy)
/// re-runs the unretrieved remainder and verifies the file. The receipt shows
/// the diagnostic, with no `unavailable:` banner and no unverified line.
#[test]
fn test_diagnostics_midrun_death_recovers_via_respawn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;
    let marker = dir.path().join("die_marker");

    let mut bridge = spawn_mockls(
        &[
            "--advertise-save",
            "--die-on",
            "textDocument/didSave",
            "--die-once-file",
            marker.to_str().context("marker path")?,
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "the bounded respawn should recover and verify the file. Got:\n{text}"
    );
    assert!(
        !text.contains("unavailable:"),
        "a recovered run carries no unavailable banner. Got:\n{text}"
    );
    assert!(
        !text.contains("unverified"),
        "a recovered file is verified, not unverified. Got:\n{text}"
    );

    Ok(())
}

/// A server that dies at every spawn degrades (decision 027, ticket 05).
/// Every process dies on `didSave` (no `--die-once-file`): the first life dies
/// mid-batch, the one bounded respawn dies again, so the file's coverage has
/// degraded. The receipt opens with the `unavailable:` banner, lists the file
/// `[unverified — …]`, never `[clean]`, and the run still exits `0`.
#[test]
fn test_diagnostics_twice_dead_degrades_with_banner() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--advertise-save", "--die-on", "textDocument/didSave"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains(&format!("unavailable: {MOCK_LANG_A}")),
        "a twice-dead server opens the receipt with the unavailable banner. Got:\n{text}"
    );
    assert!(
        text.contains("unverified"),
        "the degraded file is listed unverified. Got:\n{text}"
    );
    assert!(
        !text.contains("[clean]"),
        "degraded must never read as clean. Got:\n{text}"
    );

    Ok(())
}

/// Spawn-failure lands in the same degradation path as mid-run death (decision
/// 027, ticket 05, scope 2). A server that rejects `initialize` (the julia/r
/// "dies during the handshake" class) leaves a dead tombstone that
/// `diagnostic_servers` filters out. The file must degrade — `unavailable:`
/// banner + `[unverified — …]` — never read as `[no LSP coverage]`, and the
/// run still exits `0`.
#[test]
fn test_diagnostics_spawn_failure_degrades_with_banner() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = spawn_mockls(
        &["--fail-on", "initialize"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains(&format!("unavailable: {MOCK_LANG_A}")),
        "spawn-failure opens the receipt with the unavailable banner. Got:\n{text}"
    );
    assert!(
        text.contains("unverified"),
        "the spawn-failed file is listed unverified, not uncovered. Got:\n{text}"
    );
    assert!(
        !text.contains("[no LSP coverage]"),
        "a configured server that cannot start is a degradation, not absence. Got:\n{text}"
    );

    Ok(())
}
