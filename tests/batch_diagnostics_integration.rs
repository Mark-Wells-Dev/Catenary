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

/// A file with no language server coverage shows `[no LSP coverage]`.
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
        text.contains("zzz_no_server"),
        "Uncovered file should appear in output. Got:\n{text}"
    );
    assert!(
        text.contains("[no LSP coverage]"),
        "Uncovered file should show no LSP coverage. Got:\n{text}"
    );

    Ok(())
}

// ─── Empty batch ────────────────────────────────────────────────────

/// No files accumulated during editing — empty output (silent).
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
        text.trim().is_empty(),
        "Empty batch should return empty output. Got: {text}"
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
