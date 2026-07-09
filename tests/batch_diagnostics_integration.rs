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

use std::path::PathBuf;

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

/// A file whose language has no configured server flows free (bug 44): it is
/// not accumulated for diagnostics, so it surfaces only via the unchecked-edit
/// count, never silently dropped. Covered files still produce diagnostics.
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
    // The no-server file is gated out of accumulation (bug 44): it has no
    // configured server, so it never reaches the diagnostics batch and is not
    // rendered per-file. It is still accounted for in the unchecked-edit count
    // so the batch is not a silent, lying one.
    assert!(
        !text.contains("zzz_no_server"),
        "No-server file must not be accumulated into per-file diagnostics. Got:\n{text}"
    );
    assert!(
        text.contains("1 edit") && text.contains("not checked"),
        "No-server file should be reported as an unchecked edit. Got:\n{text}"
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

    // The good file should still produce results despite the missing sibling
    // (a clean good file would instead carry a `[clean]` line).
    assert!(
        text.contains("mock diagnostic") || text.contains("[clean]"),
        "Good file should produce output despite missing file. Got:\n{text}"
    );

    Ok(())
}

// ─── Clean files ────────────────────────────────────────────────────

/// Files a server verified with no diagnostics are listed explicitly as
/// `[clean]` — clean is stated, never silence (ws37 ticket 01).
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

    // Both verified-clean files appear with a `[clean]` line beside them.
    assert!(
        text.contains("cln_a") && text.contains("cln_b"),
        "Both clean files should be listed. Got:\n{text}"
    );
    assert!(
        text.matches("[clean]").count() == 2,
        "Each clean file should carry a `[clean]` line. Got:\n{text}"
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

// ─── Real rust-analyzer: unlinked-file regression guard (#101) ──────

/// Resolves an absolute path for a binary on $PATH.
///
/// Must be called before `isolate_env` clears PATH.
fn find_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

/// Reproduces the "unlinked-file" diagnostic scenario with real
/// rust-analyzer.
///
/// Starts with a clean project, warms up RA, then creates a new module
/// file and adds its `mod` declaration — simulating the real editing
/// workflow. Accumulates the child file before the parent to exercise
/// the worst-case `didOpen` order.
///
/// Run with: `make test-ignored T=unlinked_file_new_module`
#[test]
#[ignore = "regression guard for #101 (fixed); requires rust-analyzer"]
fn test_unlinked_file_new_module() -> Result<()> {
    let ra_bin = find_binary("rust-analyzer").context("rust-analyzer not found on PATH")?;

    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src)?;

    // Start with a clean project — no new_module yet.
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"test-unlinked\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    let main_rs = src.join("main.rs");
    std::fs::write(&main_rs, "fn main() {}\n")?;

    let root = dir.path().to_str().context("root path")?;
    let ra_path = ra_bin.to_str().context("ra path")?;
    let lsp = format!("rust:{ra_path}");

    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Warm up: force RA to fully index the project.
    let warmup = bridge.call_diagnostics(main_rs.to_str().context("main path")?)?;
    assert!(
        !warmup.contains("error"),
        "clean project should have no errors: {warmup}"
    );

    // Now simulate the agent creating a new module:
    // 1. Write new_module.rs to disk (agent uses Write tool)
    let new_module_rs = src.join("new_module.rs");
    std::fs::write(&new_module_rs, "pub fn hello() {}\n")?;

    // 2. Update main.rs with mod declaration (agent uses Edit tool)
    std::fs::write(&main_rs, "mod new_module;\n\nfn main() {}\n")?;

    // 3. done_editing with child BEFORE parent — worst-case order.
    let text = bridge.call_diagnostics_multi(&[
        new_module_rs.to_str().context("new_module path")?,
        main_rs.to_str().context("main path")?,
    ])?;

    assert_no_unlinked(&text);

    Ok(())
}

fn assert_no_unlinked(text: &str) {
    let has_unlinked = text.contains("not included in module tree")
        || text.contains("unlinked-file")
        || text.contains("file is not included");
    assert!(!has_unlinked, "unlinked-file diagnostic in output:\n{text}");
}
