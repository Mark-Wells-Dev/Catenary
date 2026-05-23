// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the batched diagnostics pipeline
//! (`process_files_batched`).
//!
//! Uses mockls to exercise the batch lifecycle: open all files → settle →
//! didSave all → settle → retrieve per file → close all.

mod common;

use anyhow::{Context, Result};

use common::BridgeProcess;

const MOCK_LANG_A: &str = "bDq7A";
const MOCK_LANG_B: &str = "bDq7B";

/// Spawns a bridge with mockls configured for `MOCK_LANG_A`.
fn spawn_mockls(mockls_args: &[&str], root: &str) -> Result<BridgeProcess> {
    let flags = mockls_args.join(" ");
    let lsp = common::mockls_lsp_arg(MOCK_LANG_A, &flags);
    BridgeProcess::spawn(&[&lsp], root)
}

// ─── Single file ────────────────────────────────────────────────────

/// Batched pipeline with one file produces the same output as the
/// sequential pipeline.
#[test]
fn test_batch_single_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&[], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Single-file batch should return diagnostics. Got: {text}"
    );

    Ok(())
}

// ─── Multi-file same server ─────────────────────────────────────────

/// Two files for the same language/server are opened before settle.
/// Diagnostics are retrieved for both.
#[test]
fn test_batch_multi_file_same_server() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("alpha.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("beta.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "echo alpha\n")?;
    std::fs::write(&file_b, "echo beta\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&[], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        file_a.to_str().context("path a")?,
        file_b.to_str().context("path b")?,
    ])?;

    // Both files should appear in the output with diagnostics.
    assert!(
        text.contains("alpha"),
        "Output should reference alpha file. Got:\n{text}"
    );
    assert!(
        text.contains("beta"),
        "Output should reference beta file. Got:\n{text}"
    );
    assert!(
        text.contains("mock diagnostic"),
        "Output should contain diagnostics. Got:\n{text}"
    );

    Ok(())
}

// ─── Multi-file different servers ───────────────────────────────────

/// Files for different languages route to different servers. Each
/// server only receives its own files.
#[test]
fn test_batch_multi_file_different_servers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("one.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("two.{MOCK_LANG_B}"));
    std::fs::write(&file_a, "echo one\n")?;
    std::fs::write(&file_b, "echo two\n")?;

    let lsp_a = common::mockls_lsp_arg(MOCK_LANG_A, "");
    let lsp_b = common::mockls_lsp_arg(MOCK_LANG_B, "");
    let root = dir.path().to_str().context("path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_a, &lsp_b], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        file_a.to_str().context("path a")?,
        file_b.to_str().context("path b")?,
    ])?;

    assert!(
        text.contains("one"),
        "Output should reference lang A file. Got:\n{text}"
    );
    assert!(
        text.contains("two"),
        "Output should reference lang B file. Got:\n{text}"
    );

    Ok(())
}

// ─── No diagnostic servers ─────────────────────────────────────────

/// A file with no language server coverage is omitted from output.
/// Covered files still produce diagnostics.
#[test]
fn test_batch_uncovered_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let covered = dir.path().join(format!("covered.{MOCK_LANG_A}"));
    let uncovered = dir.path().join("mystery.zzz_no_server");
    std::fs::write(&covered, "echo covered\n")?;
    std::fs::write(&uncovered, "no server for this\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&[], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        covered.to_str().context("path covered")?,
        uncovered.to_str().context("path uncovered")?,
    ])?;

    assert!(
        text.contains("mock diagnostic"),
        "Covered file should produce diagnostics. Got:\n{text}"
    );
    assert!(
        !text.contains("zzz_no_server"),
        "Uncovered file should be omitted entirely. Got:\n{text}"
    );
    assert!(
        !text.contains("N/A"),
        "No N/A section in output. Got:\n{text}"
    );

    Ok(())
}

// ─── Empty batch ────────────────────────────────────────────────────

/// No files accumulated during editing. Returns `[no files modified]`.
#[test]
fn test_batch_empty() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Need at least one file for the language server to exist.
    let file = dir.path().join(format!("placeholder.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&[], root)?;
    bridge.initialize()?;

    // Enter and exit editing mode with no files accumulated.
    let text = bridge.call_diagnostics_multi(&[])?;

    assert!(
        text.contains("[no files modified]"),
        "Empty batch should return [no files modified]. Got: {text}"
    );

    Ok(())
}

// ─── didSave servers ────────────────────────────────────────────────

/// Server that advertises `textDocumentSync.save` receives `didSave`
/// for all files in the batch. Flycheck runs once after all saves.
#[test]
fn test_batch_did_save_all_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("sav_a.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("sav_b.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "echo save a\n")?;
    std::fs::write(&file_b, "echo save b\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&["--advertise-save"], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        file_a.to_str().context("path a")?,
        file_b.to_str().context("path b")?,
    ])?;

    // Both files should produce diagnostics (server ran didSave for both).
    assert!(
        text.contains("sav_a") && text.contains("sav_b"),
        "Both files should appear in output. Got:\n{text}"
    );
    assert!(
        text.contains("mock diagnostic"),
        "didSave server should produce diagnostics. Got:\n{text}"
    );

    Ok(())
}

// ─── File open failure ──────────────────────────────────────────────

/// One file is unreadable (missing). Other files still produce
/// diagnostics.
#[test]
fn test_batch_file_open_failure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let good = dir.path().join(format!("good.{MOCK_LANG_A}"));
    let missing = dir.path().join(format!("missing.{MOCK_LANG_A}"));
    std::fs::write(&good, "echo good\n")?;
    // `missing` is not created — it doesn't exist on disk.

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&[], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        good.to_str().context("path good")?,
        missing.to_str().context("path missing")?,
    ])?;

    // The good file should still produce results.
    assert!(
        text.contains("mock diagnostic") || text.contains("clean"),
        "Good file should produce output despite missing file. Got:\n{text}"
    );

    Ok(())
}

// ─── Clean files ────────────────────────────────────────────────────

/// Files where the server produces no diagnostics appear in the
/// "clean" group.
#[test]
fn test_batch_clean_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("cln_a.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("cln_b.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "echo clean\n")?;
    std::fs::write(&file_b, "echo clean\n")?;

    let root = dir.path().to_str().context("path")?;
    // --no-push-diagnostics: server never publishes diagnostics.
    // Without pull support either, the result is clean.
    let mut bridge = spawn_mockls(&["--no-push-diagnostics"], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        file_a.to_str().context("path a")?,
        file_b.to_str().context("path b")?,
    ])?;

    assert!(
        text.contains("clean"),
        "Files with no diagnostics should be listed as clean. Got:\n{text}"
    );

    Ok(())
}

// ─── Pull-only server ───────────────────────────────────────────────

/// Batched pipeline with a pull-only server (no push diagnostics).
/// Diagnostics are retrieved via pull for each file in the batch.
#[test]
fn test_batch_pull_only() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("pull_a.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("pull_b.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "echo pull a\n")?;
    std::fs::write(&file_b, "echo pull b\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&["--pull-diagnostics", "--no-push-diagnostics"], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        file_a.to_str().context("path a")?,
        file_b.to_str().context("path b")?,
    ])?;

    assert!(
        text.contains("mock diagnostic"),
        "Pull-only batch should return diagnostics. Got:\n{text}"
    );

    Ok(())
}

// ─── Cross-file: all files open simultaneously ──────────────────────

/// With `--report-open-count`, the diagnostic message includes the
/// number of currently-open documents. The batch pipeline opens all
/// files before settling, so every diagnostic should report "2 open"
/// (both files open at once). A sequential pipeline would report
/// "1 open" per file.
#[test]
fn test_batch_all_files_open_simultaneously() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("cross_a.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("cross_b.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "echo cross a\n")?;
    std::fs::write(&file_b, "echo cross b\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&["--report-open-count"], root)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_multi(&[
        file_a.to_str().context("path a")?,
        file_b.to_str().context("path b")?,
    ])?;

    // Both files should report "2 open" — proving the batch pipeline
    // had both documents open when diagnostics were published.
    assert!(
        text.contains("2 open"),
        "Batch pipeline should open both files before settling. Got:\n{text}"
    );
    assert!(
        !text.contains("1 open"),
        "No file should see only 1 open (sequential behavior). Got:\n{text}"
    );

    Ok(())
}

// ─── mark_current: edited files not re-reported ─────────────────────

/// After `done_editing` completes (which calls `mark_current`), the
/// next file change diff should not report the edited files as
/// changed. Verified via the notification log: a subsequent tool
/// call triggers `notify_file_changes`, and the log should not
/// contain `didChangeWatchedFiles` for the batch-edited files.
#[test]
fn test_batch_mark_current_prevents_re_report() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("mark.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo mark\n")?;

    let notif_log = dir.path().join("notifications.jsonl");
    let notif_log_str = notif_log.to_str().context("log path")?;

    let root = dir.path().to_str().context("path")?;
    let lsp = common::mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {notif_log_str}"),
    );
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Small delay for file watcher registration to complete.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Modify the file on disk (simulates what Edit does).
    std::fs::write(&file, "echo mark changed\n")?;

    // Batch diagnostics — this calls mark_current at the end.
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Diagnostics should be returned. Got:\n{text}"
    );

    // Now trigger another tool call, which runs notify_file_changes.
    // The edited file should NOT appear as a didChangeWatchedFiles
    // event because mark_current refreshed the cache.
    let _ = bridge.call_tool_text("grep", &serde_json::json!({"pattern": "zzz_no_match"}))?;

    // Read the notification log and check for didChangeWatchedFiles
    // entries after the diagnostics pipeline ran.
    let log_content = std::fs::read_to_string(&notif_log).unwrap_or_default();
    let file_uri = format!("file://{}", file.display());

    // Count didChangeWatchedFiles entries that reference our file.
    // There may be one from the initial diff (before done_editing),
    // but there should NOT be one after (the grep call).
    let change_entries: Vec<&str> = log_content
        .lines()
        .filter(|line| line.contains("didChangeWatchedFiles") && line.contains(&file_uri))
        .collect();

    // At most one entry (from the initial notify_file_changes in the
    // batch pipeline). A second would mean mark_current failed.
    assert!(
        change_entries.len() <= 1,
        "Edited file should not be re-reported after mark_current. \
         Found {} didChangeWatchedFiles entries for {}:\n{}",
        change_entries.len(),
        file_uri,
        change_entries.join("\n")
    );

    Ok(())
}
