// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::similar_names,
    reason = "parallel server-A/server-B bindings read clearly with the _a/_b suffix"
)]
//! Integration tests for the WS31 changed-set engine (ticket 03).
//!
//! A single per-root `relpath → mtime` baseline is diffed by each coherence
//! walk; the delta is routed to each covering server filtered by what THAT
//! server registered to watch (globs + kind mask). These tests drive real
//! `didChangeWatchedFiles` registrations via `mockls --register-file-watchers`
//! and assert what each server received via `--notification-log`.

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, mockls_lsp_arg};

const MOCK_LANG_A: &str = "yX4Za";
const MOCK_LANG_B: &str = "d5apI";

/// Reads the notification log and returns every `(uri, type)` pair from every
/// `workspace/didChangeWatchedFiles` notification recorded.
fn watched_file_changes(log: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for line in log.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("method").and_then(Value::as_str) != Some("workspace/didChangeWatchedFiles") {
            continue;
        }
        let Some(changes) = entry.get("changes").and_then(Value::as_array) else {
            continue;
        };
        for change in changes {
            let uri = change
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let typ = change.get("type").and_then(Value::as_u64).unwrap_or(0);
            out.push((uri, typ));
        }
    }
    out
}

/// Counts the number of `workspace/didChangeWatchedFiles` notifications (not
/// individual changes) recorded in the log.
fn watched_file_notification_count(log: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| {
            entry.get("method").and_then(Value::as_str) == Some("workspace/didChangeWatchedFiles")
        })
        .count()
}

/// Cold baseline ⇒ the first enriched grep sends every registered-glob file
/// once as `Changed` (`FileChangeType` 2). The first walk *is* the snapshot.
#[test]
fn first_walk_sends_full_candidate_set() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "needle\n")?;
    std::fs::write(dir.path().join(format!("b.{MOCK_LANG_A}")), "other\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First enriched grep — triggers the cold-start full candidate set.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let changes = watched_file_changes(&log);
    let a_uri = format!("file://{}/a.{MOCK_LANG_A}", dir.path().display());
    let b_uri = format!("file://{}/b.{MOCK_LANG_A}", dir.path().display());

    assert!(
        changes.iter().any(|(u, t)| *u == a_uri && *t == 2),
        "first walk should announce a as Changed(2). changes={changes:?}, log:\n{log}"
    );
    assert!(
        changes.iter().any(|(u, t)| *u == b_uri && *t == 2),
        "first walk should announce b as Changed(2). changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// No FS change between two greps ⇒ zero `didChangeWatchedFiles` on the second
/// (the bug-38 no-repeat property).
#[test]
fn second_walk_sends_only_delta() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First grep: cold-start full set (at least one notification).
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    std::thread::sleep(Duration::from_millis(150));
    let log_after_first = std::fs::read_to_string(&log_path).unwrap_or_default();
    let count_after_first = watched_file_notification_count(&log_after_first);
    assert!(
        count_after_first >= 1,
        "first walk should send at least one notification, got {count_after_first}"
    );

    // Second grep with no FS change: no new notifications.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let count_total = watched_file_notification_count(&log);
    assert_eq!(
        count_total, count_after_first,
        "second walk with no FS change must send zero new notifications \
         (bug-38 no-repeat). first={count_after_first}, total={count_total}, log:\n{log}"
    );
    Ok(())
}

/// Touch one `.MOCK_LANG_A` file after the first walk; only the server whose
/// glob matches gets exactly that path. A server registering a different glob
/// gets nothing on the second walk.
#[test]
fn external_change_routed_to_matching_server_only() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_a = dir.path().join("notifications_a.jsonl");
    let log_b = dir.path().join("notifications_b.jsonl");
    let changed = dir.path().join(format!("watched.{MOCK_LANG_A}"));
    std::fs::write(&changed, "needle\n")?;
    // A file only server B's glob would match — but nothing touches it.
    std::fs::write(dir.path().join(format!("other.{MOCK_LANG_B}")), "needle\n")?;

    let log_a_arg = log_a.to_str().context("log a path")?;
    let log_b_arg = log_b.to_str().context("log b path")?;
    let lsp_a = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --notification-log {log_a_arg}"
        ),
    );
    let lsp_b = mockls_lsp_arg(
        MOCK_LANG_B,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_B} \
             --notification-log {log_b_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_a, &lsp_b], root)?;
    bridge.initialize()?;

    // First grep seeds both baselines.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    std::thread::sleep(Duration::from_millis(150));

    // Touch only the MOCK_LANG_A file (advance mtime well past first walk).
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&changed, "needle changed\n")?;

    // Second grep routes the delta.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let changed_uri = format!("file://{}/watched.{MOCK_LANG_A}", dir.path().display());

    // Server A: received the changed file on the second walk.
    let log_a_text = std::fs::read_to_string(&log_a).unwrap_or_default();
    let a_changes = watched_file_changes(&log_a_text);
    let a_changed_count = a_changes.iter().filter(|(u, _)| *u == changed_uri).count();
    assert!(
        a_changed_count >= 1,
        "matching server A should receive the changed .{MOCK_LANG_A} file. \
         a_changes={a_changes:?}, log:\n{log_a_text}"
    );

    // Server B (glob **/*.MOCK_LANG_B): must NOT receive the .MOCK_LANG_A file.
    let log_b_text = std::fs::read_to_string(&log_b).unwrap_or_default();
    let b_changes = watched_file_changes(&log_b_text);
    assert!(
        !b_changes.iter().any(|(u, _)| *u == changed_uri),
        "non-matching server B must not receive the .{MOCK_LANG_A} file. \
         b_changes={b_changes:?}, log:\n{log_b_text}"
    );
    Ok(())
}

/// A file genuinely created *after* the baseline is seeded (absent from a
/// populated baseline) routes as `Created` with wire `FileChangeType` 1 to a
/// server that registered the `Create` kind, and is suppressed for a Change-only
/// server. Exercises the true Created/Changed distinction with a populated
/// baseline (the first walk's cold snapshot is always `Changed`).
#[test]
fn created_file_routed_with_created_wire_type() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_create = dir.path().join("notifications_create.jsonl");
    let log_change = dir.path().join("notifications_change.jsonl");
    // Seed with one existing file so the first grep has a match.
    std::fs::write(dir.path().join(format!("seed.{MOCK_LANG_A}")), "needle\n")?;

    let log_create_arg = log_create.to_str().context("create log path")?;
    let log_change_arg = log_change.to_str().context("change log path")?;
    // Server A: kind 7 (ALL) — registers Create, so receives creations.
    let lsp_create = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --notification-log {log_create_arg}"
        ),
    );
    // Server B: kind 2 (Change only, no Create bit) — must suppress creations.
    let lsp_change = mockls_lsp_arg(
        MOCK_LANG_B,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 2 --notification-log {log_change_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_create, &lsp_change], root)?;
    bridge.initialize()?;

    // First grep seeds the baseline (seed.* recorded; both keys now exist).
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    std::thread::sleep(Duration::from_millis(150));

    // Create a NEW file — absent from a POPULATED baseline ⇒ a genuine `Created`.
    let created = dir.path().join(format!("created.{MOCK_LANG_A}"));
    std::fs::write(&created, "needle\n")?;

    // Second grep routes the creation.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let created_uri = format!("file://{}/created.{MOCK_LANG_A}", dir.path().display());

    // Create-registering server: receives the created path with wire type 1.
    let log_create_text = std::fs::read_to_string(&log_create).unwrap_or_default();
    let create_changes = watched_file_changes(&log_create_text);
    assert!(
        create_changes
            .iter()
            .any(|(u, t)| *u == created_uri && *t == 1),
        "Create-registering server should receive the created path as \
         Created(1). create_changes={create_changes:?}, log:\n{log_create_text}"
    );

    // Change-only server: must NOT receive the created path.
    let log_change_text = std::fs::read_to_string(&log_change).unwrap_or_default();
    let change_changes = watched_file_changes(&log_change_text);
    assert!(
        !change_changes.iter().any(|(u, _)| *u == created_uri),
        "Change-only watcher must suppress a genuinely-created path. \
         change_changes={change_changes:?}, log:\n{log_change_text}"
    );
    Ok(())
}

/// A modification to an existing (already-seeded) file routes as `Changed` with
/// wire `FileChangeType` 2 to a server that registered the `Change` kind, and is
/// suppressed for a Create-only server.
#[test]
fn changed_file_routed_with_changed_wire_type() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_change = dir.path().join("notifications_change.jsonl");
    let log_create = dir.path().join("notifications_create.jsonl");
    let existing = dir.path().join(format!("existing.{MOCK_LANG_A}"));
    std::fs::write(&existing, "needle\n")?;

    let log_change_arg = log_change.to_str().context("change log path")?;
    let log_create_arg = log_create.to_str().context("create log path")?;
    // Server A: kind 2 (Change only) — receives modifications.
    let lsp_change = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 2 --notification-log {log_change_arg}"
        ),
    );
    // Server B: kind 1 (Create only, no Change bit) — must suppress modifications.
    let lsp_create = mockls_lsp_arg(
        MOCK_LANG_B,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 1 --notification-log {log_create_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_change, &lsp_create], root)?;
    bridge.initialize()?;

    // First grep seeds the baseline (existing.* recorded as the cold snapshot).
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    std::thread::sleep(Duration::from_millis(150));

    // Modify the existing file (advance mtime past the first walk).
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&existing, "needle changed\n")?;

    // Second grep routes the modification.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let existing_uri = format!("file://{}/existing.{MOCK_LANG_A}", dir.path().display());

    // Change-registering server: receives the modification with wire type 2.
    let log_change_text = std::fs::read_to_string(&log_change).unwrap_or_default();
    let change_changes = watched_file_changes(&log_change_text);
    assert!(
        change_changes
            .iter()
            .any(|(u, t)| *u == existing_uri && *t == 2),
        "Change-registering server should receive the modified path as \
         Changed(2). change_changes={change_changes:?}, log:\n{log_change_text}"
    );

    // Create-only server: must NOT receive the modification (only the first
    // walk's cold snapshot, which is itself Changed and therefore also
    // suppressed by a Create-only mask — so it receives nothing at all).
    let log_create_text = std::fs::read_to_string(&log_create).unwrap_or_default();
    let create_changes = watched_file_changes(&log_create_text);
    assert!(
        !create_changes.iter().any(|(u, _)| *u == existing_uri),
        "Create-only watcher must suppress a modification. \
         create_changes={create_changes:?}, log:\n{log_create_text}"
    );
    Ok(())
}

/// Edited files ride document-sync (didOpen/didSave); an externally-changed
/// file rides the nudge. The edited files are NOT in the changed-set nudge.
#[test]
fn diagnostics_excludes_edited_set() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let edited = dir.path().join(format!("edited.{MOCK_LANG_A}"));
    let external = dir.path().join(format!("external.{MOCK_LANG_A}"));
    std::fs::write(&edited, "one\n")?;
    std::fs::write(&external, "one\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --advertise-save --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First diagnostics run seeds the baseline (both files recorded).
    let _ = bridge.call_diagnostics(edited.to_str().context("edited path")?)?;
    std::thread::sleep(Duration::from_millis(150));

    // Externally change `external` (advance mtime); also re-edit `edited`.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&external, "one\ntwo\n")?;
    std::fs::write(&edited, "one\nedited\n")?;

    // Second diagnostics run on `edited`: edited rides document-sync, external
    // rides the changed-set nudge.
    let _ = bridge.call_diagnostics(edited.to_str().context("edited path")?)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let changes = watched_file_changes(&log);
    let edited_uri = format!("file://{}/edited.{MOCK_LANG_A}", dir.path().display());
    let external_uri = format!("file://{}/external.{MOCK_LANG_A}", dir.path().display());

    assert!(
        changes.iter().any(|(u, _)| *u == external_uri),
        "externally-changed file should ride the changed-set nudge. \
         changes={changes:?}, log:\n{log}"
    );
    assert!(
        !changes.iter().any(|(u, _)| *u == edited_uri),
        "edited file rides document-sync and must be excluded from the nudge. \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}
