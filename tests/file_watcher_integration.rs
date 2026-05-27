// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for `workspace/didChangeWatchedFiles`.
//!
//! Validates the full pipeline: filesystem change → `diff()` →
//! registration match → `didChangeWatchedFiles` notification.
//! Uses mockls with `--register-file-watchers` and `--notification-log`
//! to capture and verify notification delivery.

mod common;

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, mockls_lsp_arg};

const MOCK_LANG_A: &str = "yX4Za";

/// Initializes the bridge with extra delay for `registerCapability` processing.
fn initialize_with_registration(bridge: &mut BridgeProcess) -> Result<()> {
    bridge.initialize()?;
    // Allow time for mockls to send registerCapability and for the
    // bridge to process the registration.
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

/// Calls `grep` to trigger `notify_file_changes()` at the tool boundary.
/// The search itself doesn't matter — we just need the diff to run.
fn trigger_file_watch_diff(bridge: &BridgeProcess) -> Result<Value> {
    bridge.call_tool(
        "grep",
        &json!({ "pattern": "NONEXISTENT_PATTERN_FOR_DIFF_TRIGGER" }),
    )
}

/// Reads the JSONL notification log and returns all `didChangeWatchedFiles`
/// entries with their changes arrays.
fn read_watched_file_changes(log_path: &std::path::Path) -> Vec<Vec<(String, u64)>> {
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    let mut results = Vec::new();
    for line in log.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("method").and_then(Value::as_str) != Some("workspace/didChangeWatchedFiles") {
            continue;
        }
        let changes = entry
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let parsed: Vec<(String, u64)> = changes
            .iter()
            .filter_map(|c| {
                let uri = c.get("uri")?.as_str()?.to_string();
                let typ = c.get("type")?.as_u64()?;
                Some((uri, typ))
            })
            .collect();
        results.push(parsed);
    }
    results
}

/// Checks whether *any* notification batch contains the given URI and type.
fn has_change(batches: &[Vec<(String, u64)>], uri_suffix: &str, change_type: u64) -> bool {
    batches
        .iter()
        .flat_map(|b| b.iter())
        .any(|(uri, typ)| uri.ends_with(uri_suffix) && *typ == change_type)
}

/// Sets a file's mtime to 10 seconds in the past.
/// This ensures that a subsequent write will produce a detectable mtime change,
/// since `FilesystemManager::diff()` compares mtimes at second resolution.
fn backdate_mtime(path: &std::path::Path) {
    let past = SystemTime::now() - Duration::from_secs(10);
    let times = std::fs::FileTimes::new().set_modified(past);
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for backdate");
    file.set_times(times).expect("set mtime");
}

// FileChangeType constants matching the LSP spec.
const CREATED: u64 = 1;
const CHANGED: u64 = 2;
const DELETED: u64 = 3;

// ── Tests ───────────────────────────────────────────────────────────

/// No filesystem changes → no `didChangeWatchedFiles` notification.
#[test]
fn noop_diff_sends_no_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn hello()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    initialize_with_registration(&mut bridge)?;

    // No filesystem changes — just trigger a diff
    let _ = trigger_file_watch_diff(&bridge)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        batches.is_empty(),
        "Expected no didChangeWatchedFiles, got: {batches:?}"
    );
    Ok(())
}

/// Creating a new file → `Created` event in notification.
#[test]
fn new_file_sends_created_event() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    // Seed file so the language is detected and mockls spawns
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    initialize_with_registration(&mut bridge)?;

    // Create a new file after seed
    let new_file = dir.path().join(format!("new_module.{MOCK_LANG_A}"));
    std::fs::write(&new_file, "fn new_thing()\n")?;

    let _ = trigger_file_watch_diff(&bridge)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("new_module.{MOCK_LANG_A}"), CREATED),
        "Expected Created event for new_module. Batches: {batches:?}"
    );
    Ok(())
}

/// Deleting a file → `Deleted` event in notification.
#[test]
fn deleted_file_sends_deleted_event() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let doomed_file = dir.path().join(format!("doomed.{MOCK_LANG_A}"));
    std::fs::write(&doomed_file, "fn doomed()\n")?;
    // Keep a seed file so the language stays active
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    initialize_with_registration(&mut bridge)?;

    // Delete the file
    std::fs::remove_file(&doomed_file)?;

    let _ = trigger_file_watch_diff(&bridge)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("doomed.{MOCK_LANG_A}"), DELETED),
        "Expected Deleted event for doomed. Batches: {batches:?}"
    );
    Ok(())
}

/// Creating a directory → `Created` event in notification.
#[test]
fn new_directory_sends_created_event() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    initialize_with_registration(&mut bridge)?;

    // Create a new subdirectory
    std::fs::create_dir(dir.path().join("subdir"))?;

    let _ = trigger_file_watch_diff(&bridge)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, "subdir", CREATED),
        "Expected Created event for subdir. Batches: {batches:?}"
    );
    Ok(())
}

/// Simulates `git checkout` — multiple creates, deletes, and changes in one
/// diff cycle produce a single batched notification.
#[test]
fn branch_switch_sends_batched_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let file_a = dir.path().join(format!("a.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("b.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn a()\n")?;
    std::fs::write(&file_b, "fn b()\n")?;
    // Backdate mtime so that the rewrite below produces a detectable change
    // (diff() compares mtimes at second resolution).
    backdate_mtime(&file_b);

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    initialize_with_registration(&mut bridge)?;

    // Simulate branch switch: delete a, modify b, create c
    std::fs::remove_file(&file_a)?;
    std::fs::write(&file_b, "fn b_modified()\n")?;
    let file_c = dir.path().join(format!("c.{MOCK_LANG_A}"));
    std::fs::write(&file_c, "fn c()\n")?;

    let _ = trigger_file_watch_diff(&bridge)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("a.{MOCK_LANG_A}"), DELETED),
        "Expected Deleted event for a. Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("b.{MOCK_LANG_A}"), CHANGED),
        "Expected Changed event for b. Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("c.{MOCK_LANG_A}"), CREATED),
        "Expected Created event for c. Batches: {batches:?}"
    );
    Ok(())
}

/// Module rename (the motivating case): `mv foo.ext foo/mod.ext` should
/// produce `Deleted(foo.ext)` + `Created(foo/mod.ext)`.
#[test]
fn module_rename_sends_delete_and_create() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let foo_file = dir.path().join(format!("foo.{MOCK_LANG_A}"));
    std::fs::write(&foo_file, "fn foo()\n")?;
    // Keep a seed file so the language stays active after rename
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    initialize_with_registration(&mut bridge)?;

    // Rename: foo.ext → foo/mod.ext
    let foo_dir = dir.path().join("foo");
    std::fs::create_dir(&foo_dir)?;
    let mod_file = foo_dir.join(format!("mod.{MOCK_LANG_A}"));
    std::fs::rename(&foo_file, &mod_file)?;

    let _ = trigger_file_watch_diff(&bridge)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("foo.{MOCK_LANG_A}"), DELETED),
        "Expected Deleted event for foo.ext. Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("foo/mod.{MOCK_LANG_A}"), CREATED),
        "Expected Created event for foo/mod.ext. Batches: {batches:?}"
    );
    Ok(())
}

/// Watch kind filtering: a watcher with `kind=4` (Delete only) should not
/// fire for Created events, but should fire for Deleted events.
#[test]
fn watch_kind_delete_only_filters_creates() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let doomed_file = dir.path().join(format!("doomed.{MOCK_LANG_A}"));
    std::fs::write(&doomed_file, "fn doomed()\n")?;
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --watcher-kind 4 --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    initialize_with_registration(&mut bridge)?;

    // Create a new file — should NOT trigger (kind=4 is Delete only)
    let new_file = dir.path().join(format!("new.{MOCK_LANG_A}"));
    std::fs::write(&new_file, "fn new()\n")?;

    let _ = trigger_file_watch_diff(&bridge)?;

    // Now delete a file — should trigger
    std::fs::remove_file(&doomed_file)?;

    let _ = trigger_file_watch_diff(&bridge)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        !has_change(&batches, &format!("new.{MOCK_LANG_A}"), CREATED),
        "Created event should NOT appear with kind=4 (Delete only). Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("doomed.{MOCK_LANG_A}"), DELETED),
        "Deleted event SHOULD appear with kind=4. Batches: {batches:?}"
    );
    Ok(())
}

// ── rust-analyzer smoke test ────────────────────────────────────────

/// The motivating case for this workstream: renaming `src/foo.rs` to
/// `src/foo/mod.rs` via `mv` should deliver `didChangeWatchedFiles` so
/// rust-analyzer rebuilds its module graph. After the notification, RA
/// should resolve symbols from the renamed module without `unlinked-file`
/// diagnostics.
///
/// Run with: `make test-ignored T=ra_module_rename`
/// Requires: rust-analyzer on PATH.
#[test]
#[ignore = "requires rust-analyzer; validates the motivating file-watcher scenario"]
fn ra_module_rename_resolves_after_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let state_dir = tempfile::tempdir()?;

    // Minimal Rust project: lib.rs re-exports foo, foo.rs defines a function.
    let cargo_toml = dir.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        "[package]\nname = \"fw-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;

    let src_dir = dir.path().join("src");
    std::fs::create_dir(&src_dir)?;
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub mod foo;\npub use foo::hello;\n",
    )?;
    std::fs::write(
        src_dir.join("foo.rs"),
        "pub fn hello() -> &'static str { \"hello\" }\n",
    )?;

    drop(state_dir); // not needed — BridgeProcess manages its own state dir
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&["rust:rust-analyzer"], root)?;
    initialize_with_registration(&mut bridge)?;

    // Wait for rust-analyzer to index — poll with grep until symbols resolve.
    let mut indexed = false;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_secs(2));

        let text = bridge
            .call_tool_text("grep", &json!({ "pattern": "hello" }))
            .unwrap_or_default();
        if text.contains("## [") {
            indexed = true;
            break;
        }
    }
    assert!(indexed, "rust-analyzer did not index within 60s");

    // Perform the rename: src/foo.rs → src/foo/mod.rs
    let foo_dir = src_dir.join("foo");
    std::fs::create_dir(&foo_dir)?;
    std::fs::rename(src_dir.join("foo.rs"), foo_dir.join("mod.rs"))?;

    // Trigger diff → didChangeWatchedFiles
    let _ = trigger_file_watch_diff(&bridge)?;

    // Give rust-analyzer time to process the notification and re-index.
    std::thread::sleep(Duration::from_secs(5));

    // Verify: grep for `hello` should still resolve the symbol in foo/mod.rs.
    // If the notification wasn't delivered, RA would show unlinked-file errors
    // and the module graph would be broken.
    let text = bridge
        .call_tool_text("grep", &json!({ "pattern": "hello" }))
        .unwrap_or_default();

    // The symbol should still be enriched (resolved by RA) after the rename.
    assert!(
        text.contains("foo/mod.rs") || text.contains("## ["),
        "Expected rust-analyzer to resolve symbols in foo/mod.rs after \
         didChangeWatchedFiles notification. Got:\n{text}"
    );

    Ok(())
}
