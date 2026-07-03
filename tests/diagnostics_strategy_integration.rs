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

use anyhow::{Context, Result};

use common::BridgeProcess;

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

    // Should either get diagnostics (if server published before dying),
    // a status message, a notify error, or empty output (a server that dies
    // before retrieval produces no result, so the file is neither `[clean]`
    // nor dirty — it stays out of the receipt). No raw infrastructure
    // messages to agent.
    let is_acceptable = text.contains("mock diagnostic")
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
                "[server.mockls-a]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [server.mockls-b]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [language.{MOCK_LANG_A}]\n\
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
                "[server.mockls-diag]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [server.mockls-nodiag]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [language.{MOCK_LANG_A}]\n\
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
                "[server.mockls-strict]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\
                 min_severity = \"error\"\n\n\
                 [server.mockls-lax]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [language.{MOCK_LANG_A}]\n\
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
                "[server.mockls-only]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [language.{MOCK_LANG_A}]\n\
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
                "[server.mockls-crash]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--drop-after\", \"3\"]\n\n\
                 [server.mockls-stable]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\"]\n\n\
                 [language.{MOCK_LANG_A}]\n\
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
