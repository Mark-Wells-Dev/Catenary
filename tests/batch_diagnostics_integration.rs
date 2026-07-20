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
//! Uses mockls to exercise the held-open batch lifecycle
//! (diagnostics-debt 01): change-gated didOpen/didChange → settle →
//! didSave the unsaved → settle → retrieve per file; documents stay open
//! across rounds and close at the owning agent's Stop/SubagentStop.

mod common;

use std::path::PathBuf;

use anyhow::{Context, Result};

use common::BridgeProcess;

// Blessed personas as the mock server keys (diagnostics-debt 04c): membership in
// the seed manifest — not the retired bless-list wildcard — is
// what makes a mock a diagnostics source. `mockls-event`'s behavior bundle is
// empty, so this rename changes zero wire behavior. `MOCK_LANG_B` is the second,
// distinct key for the one test that runs two servers at once
// (`test_batch_multi_file_different_servers`); `mockls-declared` is the standard
// second persona.
const MOCK_LANG_A: &str = "mockls-event";
const MOCK_LANG_B: &str = "mockls-declared";

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

/// A file whose language has no configured server produces no per-file
/// diagnostics (bug 44): a scoped serve naming both a covered and an uncovered
/// file diagnoses the covered one and never renders the uncovered one as a
/// clean/dirty result. (Root-ownership stage 3 retired the identity-keyed
/// skipped-edits accumulation note — the serve reads the ledger and renders what
/// it can diagnose; a no-server file simply carries no diagnostics.)
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
    // The no-server file has no configured server, so it never renders as a
    // clean/dirty per-file diagnostic line — it carries no diagnostics to show.
    assert!(
        !text.contains("mystery.zzz_no_server:"),
        "No-server file must not render as a per-file diagnostic. Got:\n{text}"
    );
    assert!(
        !text.contains("outside tracked roots"),
        "An in-root no-server file must not be misattributed to root coverage (misc 173). Got:\n{text}"
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

// ─── Held-open batch lifecycle (diagnostics-debt 01) ────────────────
//
// The per-round atomic didOpen→…→didClose cycle is retired: a batch file is
// opened once per connection, change-synced only when its disk content moved
// (mtime fast-path, hash tie-break), saved only when its last-sent content is
// unsaved, and closed only at the owning agent's Stop/SubagentStop. These
// tests read mockls's `--notification-log` and assert **deltas** between
// rounds — the spawn-time eager health probe opens the first matching file in
// the root and leaves it held open (bug 133 lean 2: no probe `didClose`, so
// no close-clear is ever owed), so absolute counts are ambiguous by design.

/// Counts notification-log entries with `method` addressed to `uri`.
fn count_doc_notifications(log: &str, method: &str, uri: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|e| {
            e.get("method").and_then(serde_json::Value::as_str) == Some(method)
                && e.get("uri").and_then(serde_json::Value::as_str) == Some(uri)
        })
        .count()
}

/// The last logged document version for `(method, uri)`, if any
/// (mockls logs `version` for didOpen/didChange).
fn last_doc_version(log: &str, method: &str, uri: &str) -> Option<i64> {
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|e| {
            e.get("method").and_then(serde_json::Value::as_str) == Some(method)
                && e.get("uri").and_then(serde_json::Value::as_str) == Some(uri)
        })
        .filter_map(|e| e.get("version").and_then(serde_json::Value::as_i64))
        .next_back()
}

/// Polls the merged notification log until `pred` holds (or the generous
/// backstop trips), returning the snapshot. Positive signals gate the
/// assertion; the caller still asserts on the snapshot so a backstop trip
/// fails loudly.
fn poll_merged_log_until(base: &std::path::Path, mut pred: impl FnMut(&str) -> bool) -> String {
    let deadline = std::time::Instant::now() + common::POLL_BACKSTOP;
    loop {
        let log = common::read_merged_log(base);
        if pred(&log) || std::time::Instant::now() >= deadline {
            return log;
        }
        std::thread::sleep(common::POLL_SPACING);
    }
}

/// A file URI exactly as the daemon derives it (canonical path).
fn doc_uri(path: &std::path::Path) -> Result<String> {
    Ok(format!("file://{}", path.canonicalize()?.display()))
}

/// Unit tier 1: an unchanged file's repeat round sends **no sync traffic** —
/// no reopen, no didChange, no didSave, no close — and the receipt still
/// carries the held evidence (the cached publish survives between rounds).
#[test]
fn held_open_unchanged_round_sends_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logdir = tempfile::tempdir()?;
    let nlog = logdir.path().join("notifications.jsonl");
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&["--advertise-save", "--notification-log", nlog_arg], root)?;
    bridge.initialize()?;
    let uri = doc_uri(&file)?;

    let first = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        first.contains("mock diagnostic"),
        "round 1 reports the publish. Got: {first}"
    );
    let log1 = common::read_merged_log(&nlog);
    let opens = count_doc_notifications(&log1, "textDocument/didOpen", &uri);
    let changes = count_doc_notifications(&log1, "textDocument/didChange", &uri);
    let saves = count_doc_notifications(&log1, "textDocument/didSave", &uri);
    let closes = count_doc_notifications(&log1, "textDocument/didClose", &uri);
    assert!(opens >= 1, "round 1 opens the file; log:\n{log1}");

    // Round 2, file untouched: the change gate says unchanged+saved.
    let second = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        second.contains("mock diagnostic"),
        "the repeat receipt serves the held evidence. Got: {second}"
    );
    let log2 = common::read_merged_log(&nlog);
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didOpen", &uri),
        opens,
        "an unchanged round must not reopen; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didChange", &uri),
        changes,
        "an unchanged round must not didChange; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didSave", &uri),
        saves,
        "an unchanged round must not didSave; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didClose", &uri),
        closes,
        "the batch never closes between rounds; log:\n{log2}"
    );

    Ok(())
}

/// Unit tier 2: a changed file's round sends `didChange` with the **next real
/// version** (monotonic, not a per-round stamp) followed by `didSave` — and
/// still no reopen, no close.
#[test]
fn held_open_changed_round_sends_didchange_and_didsave() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logdir = tempfile::tempdir()?;
    let nlog = logdir.path().join("notifications.jsonl");
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&["--advertise-save", "--notification-log", nlog_arg], root)?;
    bridge.initialize()?;
    let uri = doc_uri(&file)?;

    let _ = bridge.call_diagnostics(file.to_str().context("path")?)?;
    let log1 = common::read_merged_log(&nlog);
    let opens = count_doc_notifications(&log1, "textDocument/didOpen", &uri);
    let changes = count_doc_notifications(&log1, "textDocument/didChange", &uri);
    let saves = count_doc_notifications(&log1, "textDocument/didSave", &uri);
    let closes = count_doc_notifications(&log1, "textDocument/didClose", &uri);
    let open_version =
        last_doc_version(&log1, "textDocument/didOpen", &uri).context("didOpen version")?;
    // Round 1 may itself have didChanged (the eager probe opened this file at
    // spawn, and the first serve demand re-syncs a probe-opened document —
    // bug 133 lean 2). The "next real version" baseline is the last version
    // round 1 sent, whichever leg sent it.
    let base_version =
        last_doc_version(&log1, "textDocument/didChange", &uri).unwrap_or(open_version);

    // The edit: disk content moves between rounds.
    std::fs::write(&file, "echo changed\necho line2\n")?;
    let second = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        second.contains("mock diagnostic"),
        "round 2 reports fresh diagnostics. Got: {second}"
    );

    let log2 = common::read_merged_log(&nlog);
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didOpen", &uri),
        opens,
        "a changed round didChanges, never reopens; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didChange", &uri),
        changes + 1,
        "exactly one didChange for the moved content; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didSave", &uri),
        saves + 1,
        "the didChange is followed by its didSave; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didClose", &uri),
        closes,
        "the batch never closes between rounds; log:\n{log2}"
    );
    let change_version =
        last_doc_version(&log2, "textDocument/didChange", &uri).context("didChange version")?;
    assert_eq!(
        change_version,
        base_version + 1,
        "the didChange carries the next real document version; log:\n{log2}"
    );

    Ok(())
}

/// Unit tier 5: an **out-of-band** write to a held-open document — no hook
/// tracked it — is detected at round start by the mtime+hash check (servers
/// deliver no watched-files for open documents, so the round-start check IS
/// the detection) and relayed as `didChange`+`didSave`.
#[test]
fn out_of_band_write_detected_at_round_start() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logdir = tempfile::tempdir()?;
    let nlog = logdir.path().join("notifications.jsonl");
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&["--advertise-save", "--notification-log", nlog_arg], root)?;
    bridge.initialize()?;
    let uri = doc_uri(&file)?;

    let _ = bridge.call_diagnostics(file.to_str().context("path")?)?;
    let log1 = common::read_merged_log(&nlog);
    let changes = count_doc_notifications(&log1, "textDocument/didChange", &uri);
    let saves = count_doc_notifications(&log1, "textDocument/didSave", &uri);
    let opens = count_doc_notifications(&log1, "textDocument/didOpen", &uri);

    // Out-of-band: the file changes on disk with NO hook-tracked edit, then a
    // repeat scoped run re-diagnoses it. The held-open document (opened by the
    // first run, still in the connection's open set) has its disk change detected
    // at round start (root-ownership stage 3: the serve names the file directly —
    // the retired two-phase handoff is gone).
    std::fs::write(&file, "echo out of band\n")?;
    let receipt = bridge.call_diagnostics(file.to_str().context("path")?)?;
    assert!(
        receipt.contains("mock diagnostic"),
        "the repeat run re-diagnoses the file. Got: {receipt}"
    );

    let log2 = common::read_merged_log(&nlog);
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didChange", &uri),
        changes + 1,
        "the round-start mtime+hash check detects the out-of-band write; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didSave", &uri),
        saves + 1,
        "the relayed content is saved so on-save analyzers re-check it; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didOpen", &uri),
        opens,
        "detection relays didChange, never a reopen; log:\n{log2}"
    );

    Ok(())
}

/// Unit tier 3 (re-keyed for root-ownership stage 3): a Stop/SubagentStop no
/// longer closes held-open documents by identity — that identity-correlation was
/// demolished. Documents a diagnose round opens are tagged with their ROOT and
/// close at root retirement (worktree removal) or daemon death, never at an
/// agent's Stop. This guards the demolition: an allowed Stop must leave the
/// held-open document OPEN (no `didClose`), so the one-cook-per-kitchen root's
/// documents survive until the kitchen itself retires.
#[test]
fn stop_does_not_close_held_open_docs_by_identity() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logdir = tempfile::tempdir()?;
    let nlog = logdir.path().join("notifications.jsonl");
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let file = dir.path().join(format!("held.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo one\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(&["--advertise-save", "--notification-log", nlog_arg], root)?;
    bridge.initialize()?;
    let uri = doc_uri(&file)?;
    let socket = bridge.wait_for_ipc_socket()?;

    // A diagnose serve opens the document (held open across rounds).
    let _ = bridge.call_diagnostics(file.to_str().context("path")?)?;
    let log0 = common::read_merged_log(&nlog);
    let opens = count_doc_notifications(&log0, "textDocument/didOpen", &uri);
    let closes = count_doc_notifications(&log0, "textDocument/didClose", &uri);
    assert!(
        opens >= 1,
        "the diagnose serve opened the document; log:\n{log0}"
    );

    // An allowed Stop reaches the daemon. Under stage 3 it triggers NO
    // identity-keyed document close — the held-open document must survive.
    common::ipc_request(
        &socket,
        &serde_json::json!({
            "method": "post-agent/require-release",
            "agent_id": "",
            "stop_hook_active": false,
        }),
    )?;

    // Give any (erroneous) background close a chance to land, then confirm none
    // did — the close count is unchanged and the document is still open.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let log1 = common::read_merged_log(&nlog);
    assert_eq!(
        count_doc_notifications(&log1, "textDocument/didClose", &uri),
        closes,
        "an allowed Stop must NOT close the held-open doc by identity (stage 3); log:\n{log1}"
    );

    Ok(())
}

/// Unit tier 4 (the query seam, unchanged leg): a grep/glob query against a
/// held-open document neither reopens nor closes it — and, unchanged on
/// disk, relays nothing at all.
#[test]
fn query_against_held_open_doc_neither_reopens_nor_closes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logdir = tempfile::tempdir()?;
    let nlog = logdir.path().join("notifications.jsonl");
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let file = dir.path().join(format!("seam.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn seam_probe\nseam_probe\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(
        &[
            "--advertise-save",
            "--register-file-watchers",
            "--notification-log",
            nlog_arg,
        ],
        root,
    )?;
    bridge.initialize()?;
    let uri = doc_uri(&file)?;

    // The batch holds the doc open.
    let _ = bridge.call_diagnostics(file.to_str().context("path")?)?;
    let log1 = common::read_merged_log(&nlog);
    let opens = count_doc_notifications(&log1, "textDocument/didOpen", &uri);
    let changes = count_doc_notifications(&log1, "textDocument/didChange", &uri);
    let closes = count_doc_notifications(&log1, "textDocument/didClose", &uri);

    // An enriched query touches the file — no lifecycle traffic results.
    let text = bridge.call_tool_text("grep", &serde_json::json!({ "pattern": "seam_probe" }))?;
    assert!(
        text.contains("seam_probe"),
        "the query itself serves. Got:\n{text}"
    );

    let log2 = common::read_merged_log(&nlog);
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didOpen", &uri),
        opens,
        "a query never reopens a held-open doc; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didClose", &uri),
        closes,
        "a query never closes a held-open doc; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didChange", &uri),
        changes,
        "an unchanged held-open doc gets no relay from a query; log:\n{log2}"
    );

    Ok(())
}

/// Unit tier 4 (the query seam, changed leg — the watch-before-query
/// invariant's second dispatch form): pending disk knowledge for an **open**
/// document is delivered before the query as the didChange full-text relay,
/// never as `didChangeWatchedFiles` — and still no reopen, no close.
#[test]
fn query_relays_didchange_for_changed_held_open_doc() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logdir = tempfile::tempdir()?;
    let nlog = logdir.path().join("notifications.jsonl");
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let file = dir.path().join(format!("relay.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn relay_probe\nrelay_probe\n")?;

    let root = dir.path().to_str().context("path")?;
    let mut bridge = spawn_mockls(
        &[
            "--advertise-save",
            "--register-file-watchers",
            "--notification-log",
            nlog_arg,
        ],
        root,
    )?;
    bridge.initialize()?;
    let uri = doc_uri(&file)?;

    // The batch holds the doc open.
    let _ = bridge.call_diagnostics(file.to_str().context("path")?)?;
    let log1 = common::read_merged_log(&nlog);
    let opens = count_doc_notifications(&log1, "textDocument/didOpen", &uri);
    let changes = count_doc_notifications(&log1, "textDocument/didChange", &uri);
    let closes = count_doc_notifications(&log1, "textDocument/didClose", &uri);

    // The disk moves out-of-band; the next query must deliver that knowledge
    // to the open doc as a didChange relay (watch-before-query).
    common::rewrite_advancing_mtime(&file, "fn relay_probe\nfn relay_extra\nrelay_probe\n")?;
    let _ = bridge.call_tool_text("grep", &serde_json::json!({ "pattern": "relay_probe" }))?;

    let log2 = poll_merged_log_until(&nlog, |log| {
        count_doc_notifications(log, "textDocument/didChange", &uri) > changes
    });
    assert!(
        count_doc_notifications(&log2, "textDocument/didChange", &uri) > changes,
        "the changed open doc is relayed as didChange before the query; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didOpen", &uri),
        opens,
        "the relay is a didChange, never a reopen; log:\n{log2}"
    );
    assert_eq!(
        count_doc_notifications(&log2, "textDocument/didClose", &uri),
        closes,
        "the query never closes the held-open doc; log:\n{log2}"
    );
    // The open doc must NOT be routed as watched-files — servers ignore
    // those for open documents (the invariant's two dispatch forms).
    let watched = common::watched_file_changes(&log2);
    assert!(
        !watched.iter().any(|(u, _)| u == &uri),
        "an open doc never routes as didChangeWatchedFiles; got {watched:?}"
    );

    Ok(())
}
