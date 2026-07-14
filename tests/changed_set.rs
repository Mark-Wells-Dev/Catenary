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

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{
    BridgeProcess, mockls_lsp_arg, poll_log_until, read_merged_log, rewrite_advancing_mtime,
    wait_for_change, watched_file_notification_count,
};

// Blessed personas as the mock server keys (diagnostics-debt 04c): manifest
// membership — not the retired bless-list wildcard — is what makes
// a mock a covering diagnostics source. `mockls-event`'s behavior bundle is empty,
// so this rename changes zero wire behavior; these tests assert on
// `didChangeWatchedFiles` routing (not diagnostics content), so the persona's
// push discipline is irrelevant here. `MOCK_LANG_B` is the second, distinct key
// for the tests that run two servers at once; `mockls-declared` is the standard
// second persona.
const MOCK_LANG_A: &str = "mockls-event";
const MOCK_LANG_B: &str = "mockls-declared";

/// Cold baseline ⇒ the first enriched grep sends every registered-glob file
/// once as `Changed` (`FileChangeType` 2). The first walk *is* the snapshot.
#[test]
fn first_walk_sends_full_candidate_set() -> Result<()> {
    let dir = common::canonical_tempdir()?;
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

    let a_uri = format!("file://{}/a.{MOCK_LANG_A}", dir.path().display());
    let b_uri = format!("file://{}/b.{MOCK_LANG_A}", dir.path().display());

    // Poll the live log until BOTH a and b are announced (the positive completion
    // signal that the cold walk flushed to mockls) instead of a fixed sleep.
    let changes = poll_log_until(&log_path, |c| {
        c.iter().any(|(u, t)| *u == a_uri && *t == 2)
            && c.iter().any(|(u, t)| *u == b_uri && *t == 2)
    });
    let log = read_merged_log(&log_path);

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
    let dir = common::canonical_tempdir()?;
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

    let a_uri = format!("file://{}/a.{MOCK_LANG_A}", dir.path().display());

    // First grep: cold-start full set. Poll the live log until `a` is announced
    // (the positive completion signal it flushed), then record the notification
    // count — no fixed sleep guessing the flush is done.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    poll_log_until(&log_path, |c| c.iter().any(|(u, _)| *u == a_uri));
    let log_after_first = read_merged_log(&log_path);
    let count_after_first = watched_file_notification_count(&log_after_first);
    assert!(
        count_after_first >= 1,
        "first walk should send at least one notification, got {count_after_first}"
    );

    // Second grep with no FS change: must send zero new notifications.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // "No new notification" is an absence (un-pollable). Anchor on a positive FIFO
    // completion signal: a THIRD walk over a genuinely-new `tail` file routes one
    // new `didChangeWatchedFiles`. The single mockls input pipe is FIFO and mockls
    // is single-threaded, so once `tail`'s notification is logged, any notification
    // the no-change walk #2 had (wrongly) emitted — earlier on the same pipe — is
    // already written too. The total notification count must then be EXACTLY
    // count_after_first + 1 (only walk #3's), proving walk #2 emitted zero.
    let tail = dir.path().join(format!("tail.{MOCK_LANG_A}"));
    std::fs::write(&tail, "needle\n")?;
    let tail_uri = format!("file://{}/tail.{MOCK_LANG_A}", dir.path().display());
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    wait_for_change(&log_path, &tail_uri, 1);

    let log = read_merged_log(&log_path);
    let count_total = watched_file_notification_count(&log);
    assert_eq!(
        count_total,
        count_after_first + 1,
        "the no-change walk #2 must send zero new notifications; only walk #3's \
         single tail notification may be added after the first walk's count \
         (bug-38 no-repeat). first={count_after_first}, total={count_total}, log:\n{log}"
    );
    Ok(())
}

/// Touch one `.MOCK_LANG_A` file after the first walk; only the server whose
/// glob matches gets exactly that path. A server registering a different glob
/// gets nothing on the second walk.
#[test]
fn external_change_routed_to_matching_server_only() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_a = dir.path().join("notifications_a.jsonl");
    let log_b = dir.path().join("notifications_b.jsonl");
    let changed = dir.path().join(format!("watched.{MOCK_LANG_A}"));
    let b_anchor = dir.path().join(format!("other.{MOCK_LANG_B}"));
    std::fs::write(&changed, "needle\n")?;
    // Server B's file: touched on walk #2 too, so B receives its OWN walk-#2
    // change — the positive completion signal that anchors B's (absent) receipt
    // of the .A file below (an absence cannot be polled directly).
    std::fs::write(&b_anchor, "needle\n")?;

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

    // First grep seeds both baselines (recorded synchronously inside the call).
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // Modify BOTH the .A file (routes to A) and the .B file (routes to B). Each
    // rewrite gates on an observed mtime advance — the change signal — instead of
    // a fixed sleep to span mtime granularity.
    rewrite_advancing_mtime(&changed, "needle changed\n")?;
    rewrite_advancing_mtime(&b_anchor, "needle changed\n")?;

    // Second grep routes the deltas to each matching server.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let changed_uri = format!("file://{}/watched.{MOCK_LANG_A}", dir.path().display());
    let b_anchor_uri = format!("file://{}/other.{MOCK_LANG_B}", dir.path().display());

    // Server A: poll its live log until it records the changed .A file (positive
    // completion signal), then assert.
    let a_changes = wait_for_change(&log_a, &changed_uri, 2);
    let log_a_text = read_merged_log(&log_a);
    let a_changed_count = a_changes.iter().filter(|(u, _)| *u == changed_uri).count();
    assert!(
        a_changed_count >= 1,
        "matching server A should receive the changed .{MOCK_LANG_A} file. \
         a_changes={a_changes:?}, log:\n{log_a_text}"
    );

    // Server B: poll its live log until it records its OWN walk-#2 change
    // (other.<LANG_B>) — proving B's walk-#2 routing ran and flushed — then assert
    // in that SAME snapshot that B did NOT receive the .A file.
    let b_changes = wait_for_change(&log_b, &b_anchor_uri, 2);
    let log_b_text = read_merged_log(&log_b);
    assert!(
        b_changes.iter().any(|(u, _)| *u == b_anchor_uri),
        "server B should receive its own .{MOCK_LANG_B} change (the anchor proving \
         B's walk ran). b_changes={b_changes:?}, log:\n{log_b_text}"
    );
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
    let dir = common::canonical_tempdir()?;
    let log_create = dir.path().join("notifications_create.jsonl");
    let log_change = dir.path().join("notifications_change.jsonl");
    // Seed with one existing file so the first grep has a match. It is modified on
    // walk #2 to give the Change-only server B a Changed(2) it accepts — the
    // positive completion signal anchoring B's (suppressed) receipt of the
    // created path below (an absence cannot be polled).
    let seed = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed, "needle\n")?;
    // A `.MOCK_LANG_B` file so the daemon DETECTS and SPAWNS server B (spawn_all
    // only spawns languages with matching files). Without it B never spawns, so
    // it can never be a covering watcher and the kind-mask suppression below would
    // be vacuous (the original test was: B's empty log trivially lacked the path).
    // B's watcher glob is `.MOCK_LANG_A`, so this file is not itself routed to B.
    std::fs::write(dir.path().join(format!("spawn.{MOCK_LANG_B}")), "needle\n")?;

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

    // Create a NEW file — absent from a POPULATED baseline ⇒ a genuine `Created`.
    let created = dir.path().join(format!("created.{MOCK_LANG_A}"));
    std::fs::write(&created, "needle\n")?;
    // Modify the seed too, so the Change-only server B has a Changed(2) anchor.
    rewrite_advancing_mtime(&seed, "needle changed\n")?;

    // Second grep routes the creation (and the seed modification).
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let created_uri = format!("file://{}/created.{MOCK_LANG_A}", dir.path().display());
    let seed_uri = format!("file://{}/seed.{MOCK_LANG_A}", dir.path().display());

    // Create-registering server A: poll until it records the created path as
    // Created(1) — positive completion signal — then assert.
    let create_changes = wait_for_change(&log_create, &created_uri, 1);
    let log_create_text = read_merged_log(&log_create);
    assert!(
        create_changes
            .iter()
            .any(|(u, t)| *u == created_uri && *t == 1),
        "Create-registering server should receive the created path as \
         Created(1). create_changes={create_changes:?}, log:\n{log_create_text}"
    );

    // Change-only server B: poll until it records the seed's Changed(2) (proving
    // B's walk-#2 routing ran and flushed), then assert in that SAME snapshot that
    // B did NOT receive the created path.
    let change_changes = wait_for_change(&log_change, &seed_uri, 2);
    let log_change_text = read_merged_log(&log_change);
    assert!(
        change_changes
            .iter()
            .any(|(u, t)| *u == seed_uri && *t == 2),
        "Change-only server should receive the seed Changed(2) anchor. \
         change_changes={change_changes:?}, log:\n{log_change_text}"
    );
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
    let dir = common::canonical_tempdir()?;
    let log_change = dir.path().join("notifications_change.jsonl");
    let log_create = dir.path().join("notifications_create.jsonl");
    let existing = dir.path().join(format!("existing.{MOCK_LANG_A}"));
    std::fs::write(&existing, "needle\n")?;
    // A `.MOCK_LANG_B` file so the daemon DETECTS and SPAWNS server B (spawn_all
    // only spawns languages with matching files). Without it B never spawns, so
    // the Create-only kind-mask suppression below would be vacuous. B's watcher
    // glob is `.MOCK_LANG_A`, so this file is not itself routed to B.
    std::fs::write(dir.path().join(format!("spawn.{MOCK_LANG_B}")), "needle\n")?;

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

    // Modify the existing file (gate on the observed mtime advance, not a sleep).
    rewrite_advancing_mtime(&existing, "needle changed\n")?;
    // Create a NEW file too, so the Create-only server B has a Created(1) anchor
    // it accepts — the positive completion signal for B's (suppressed) receipt of
    // the modification below (an absence cannot be polled).
    let fresh = dir.path().join(format!("fresh.{MOCK_LANG_A}"));
    std::fs::write(&fresh, "needle\n")?;

    // Second grep routes the modification (to A) and the creation (to B).
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let existing_uri = format!("file://{}/existing.{MOCK_LANG_A}", dir.path().display());
    let fresh_uri = format!("file://{}/fresh.{MOCK_LANG_A}", dir.path().display());

    // Change-registering server A: poll until it records the modification as
    // Changed(2) — positive completion signal — then assert.
    let change_changes = wait_for_change(&log_change, &existing_uri, 2);
    let log_change_text = read_merged_log(&log_change);
    assert!(
        change_changes
            .iter()
            .any(|(u, t)| *u == existing_uri && *t == 2),
        "Change-registering server should receive the modified path as \
         Changed(2). change_changes={change_changes:?}, log:\n{log_change_text}"
    );

    // Create-only server B: poll until it records the fresh file's Created(1)
    // (proving B's walk-#2 routing ran and flushed), then assert in that SAME
    // snapshot that B did NOT receive the modification.
    let create_changes = wait_for_change(&log_create, &fresh_uri, 1);
    let log_create_text = read_merged_log(&log_create);
    assert!(
        create_changes
            .iter()
            .any(|(u, t)| *u == fresh_uri && *t == 1),
        "Create-only server should receive the fresh Created(1) anchor. \
         create_changes={create_changes:?}, log:\n{log_create_text}"
    );
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
    let dir = common::canonical_tempdir()?;
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

    // First diagnostics run seeds the baseline (both files recorded synchronously).
    let _ = bridge.call_diagnostics(edited.to_str().context("edited path")?)?;

    // Externally change `external`; also re-edit `edited`. Each rewrite gates on an
    // observed mtime advance — the change signal — not a fixed sleep.
    rewrite_advancing_mtime(&external, "one\ntwo\n")?;
    rewrite_advancing_mtime(&edited, "one\nedited\n")?;

    // Second diagnostics run on `edited`: edited rides document-sync, external
    // rides the changed-set nudge.
    let _ = bridge.call_diagnostics(edited.to_str().context("edited path")?)?;

    let edited_uri = format!("file://{}/edited.{MOCK_LANG_A}", dir.path().display());
    let external_uri = format!("file://{}/external.{MOCK_LANG_A}", dir.path().display());

    // Poll the live log until the externally-changed file rides the nudge (the
    // positive completion signal), then assert in that SAME snapshot that the
    // edited file (which rides document-sync, not the nudge) is absent.
    let changes = wait_for_change(&log_path, &external_uri, 2);
    let log = read_merged_log(&log_path);

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

// ── Walk-breadth pre-check gate (ticket 04) ──────────────────────────────

/// `--count` grep is `WalkBreadth::None`: a dumb `grep -c` tally that skips the
/// engine entirely. No `workspace/didChangeWatchedFiles` is sent even though a
/// covering server is registered.
#[test]
fn count_grep_does_no_coherence_walk() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let a = dir.path().join(format!("a.{MOCK_LANG_A}"));
    std::fs::write(&a, "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let a_uri = format!("file://{}/a.{MOCK_LANG_A}", dir.path().display());

    // Normal enriched grep FIRST: routes `a` Changed(2) (cold walk). Poll until it
    // lands — the baseline notification count.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    poll_log_until(&log_path, |c| c.iter().any(|(u, _)| *u == a_uri));
    let baseline = watched_file_notification_count(&read_merged_log(&log_path));

    // A `--count` grep — must NOT run the engine (None breadth), so it must add
    // ZERO notifications.
    let resp =
        bridge.call_search_raw("tool/grep", &json!({ "pattern": "needle", "count": true }))?;
    assert!(
        resp.get("matches").and_then(Value::as_u64).is_some(),
        "count grep should return a numeric match tally: {resp:?}"
    );

    // "Count grep sent nothing" is an absence (un-pollable). Anchor on a positive
    // FIFO signal AFTER it: a NEW file `b` + a normal grep routes one `b` Created(1).
    // The single mockls pipe is FIFO and mockls single-threaded, so once `b` is
    // logged, any notification the count grep (wrongly) sent — earlier on the pipe
    // — is already written. The total must then be EXACTLY baseline + 1 (only the
    // `b` notification), proving the count grep added zero.
    let b = dir.path().join(format!("b.{MOCK_LANG_A}"));
    std::fs::write(&b, "needle\n")?;
    let b_uri = format!("file://{}/b.{MOCK_LANG_A}", dir.path().display());
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    wait_for_change(&log_path, &b_uri, 1);

    let log = read_merged_log(&log_path);
    let count = watched_file_notification_count(&log);
    assert_eq!(
        count,
        baseline + 1,
        "--count grep is WalkBreadth::None and must send zero \
         didChangeWatchedFiles (only the bracketing normal greps' notifications \
         appear). baseline={baseline}, count={count}, log:\n{log}"
    );
    Ok(())
}

/// A query whose covering server registered **no** file watchers is
/// `WalkBreadth::None` (the `(no LSP)` coverage case): the engine is skipped, so
/// no nudge is sent. The server still runs — it just registered nothing to
/// watch, so a coherence walk would route nothing.
#[test]
fn no_lsp_query_no_nudge() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // No --register-file-watchers ⇒ no covering watchers ⇒ has_covering_watchers
    // is false ⇒ WalkBreadth::None.
    let lsp = mockls_lsp_arg(MOCK_LANG_A, &format!("--notification-log {log_arg}"));
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Enriched grep, but no covering watcher ⇒ no nudge.
    //
    // This server registered NO file watchers, so it can NEVER receive a
    // `workspace/didChangeWatchedFiles` — there is no positive notification to
    // anchor an absence poll on, and none is needed: the property is doubly
    // guaranteed (the engine is skipped because `has_covering_watchers` is false,
    // AND routing filters by registered watchers, of which there are zero). The
    // grep call returns synchronously only after that decision, with nothing sent
    // over the pipe — so the count is final the instant it returns. No fixed
    // sleep, and (uniquely) no anchor is possible or required.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let log = read_merged_log(&log_path);
    let count = watched_file_notification_count(&log);
    assert_eq!(
        count, 0,
        "a query with no covering file-watcher is WalkBreadth::None and must \
         send zero didChangeWatchedFiles. log:\n{log}"
    );
    Ok(())
}

// ── Deletion reaping — full walks only (ticket 04) ───────────────────────

/// Enriched grep is a `Full` walk: after seeding the baseline, deleting a
/// tracked `.MOCK_LANG_A` file and re-running grep reaps it — the covering
/// server receives `Deleted` (wire `FileChangeType` 3) for the gone file.
#[test]
fn enriched_grep_full_walk_reaps_deletion() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let keep = dir.path().join(format!("keep.{MOCK_LANG_A}"));
    let gone = dir.path().join(format!("gone.{MOCK_LANG_A}"));
    std::fs::write(&keep, "needle\n")?;
    std::fs::write(&gone, "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 7 (ALL) ⇒ registers Delete, so it receives reaped deletions.
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First grep seeds the baseline (both files recorded synchronously).
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // Delete the tracked file, then run grep again — the full walk reaps it.
    std::fs::remove_file(&gone)?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let gone_uri = format!("file://{}/gone.{MOCK_LANG_A}", dir.path().display());

    // Poll the live log until the reaped Deleted(3) lands (positive completion
    // signal), then assert. No fixed sleep, no shutdown/flush race.
    let changes = wait_for_change(&log_path, &gone_uri, 3);
    let log = read_merged_log(&log_path);
    assert!(
        changes.iter().any(|(u, t)| *u == gone_uri && *t == 3),
        "a full enriched-grep walk must reap the deleted file as Deleted(3). \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// `catenary diagnostics` is a `Full` walk: an externally-deleted tracked file
/// (not in the edited-set) is reaped as `Deleted` (wire `FileChangeType` 3).
#[test]
fn diagnostics_full_walk_reaps_deletion() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let edited = dir.path().join(format!("edited.{MOCK_LANG_A}"));
    let gone = dir.path().join(format!("gone.{MOCK_LANG_A}"));
    std::fs::write(&edited, "one\n")?;
    std::fs::write(&gone, "one\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --advertise-save --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First diagnostics run seeds the baseline (both files recorded synchronously).
    let _ = bridge.call_diagnostics(edited.to_str().context("edited path")?)?;

    // Externally delete the tracked file (not the edited one).
    std::fs::remove_file(&gone)?;

    // Second diagnostics run on `edited`: the full stat-walk reaps `gone`.
    let _ = bridge.call_diagnostics(edited.to_str().context("edited path")?)?;

    let gone_uri = format!("file://{}/gone.{MOCK_LANG_A}", dir.path().display());

    // Poll the live log until the reaped Deleted(3) lands (positive completion
    // signal), then assert. No fixed sleep, no shutdown/flush race.
    let changes = wait_for_change(&log_path, &gone_uri, 3);
    let log = read_merged_log(&log_path);
    assert!(
        changes.iter().any(|(u, t)| *u == gone_uri && *t == 3),
        "a full diagnostics walk must reap the deleted file as Deleted(3). \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// A Delete-only watcher (kind 4 — no Create/Change bit) must still get its
/// matched files into the per-root baseline while present, so a later full walk
/// can reap their deletion. On the first walk (file present) the Delete-only
/// server receives NOTHING — it didn't ask for Create/Change, so routing
/// kind-filters the cold `Changed` snapshot away. After deletion the second full
/// walk reaps the gone file as `Deleted` (wire `FileChangeType` 3).
#[test]
fn delete_only_watcher_reaps_deletion() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    // `keep` survives so the post-deletion grep still matches a file under the
    // root, running the full walk that reaps `gone`.
    let keep = dir.path().join(format!("keep.{MOCK_LANG_A}"));
    let gone = dir.path().join(format!("gone.{MOCK_LANG_A}"));
    std::fs::write(&keep, "needle\n")?;
    std::fs::write(&gone, "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 4 (Delete only) — no Create/Change bit. Files must still be baselined
    // while present so the reaping sweep can later emit their deletion.
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 4 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let gone_uri = format!("file://{}/gone.{MOCK_LANG_A}", dir.path().display());

    // First grep seeds the baseline (the file IS baselined even though this
    // watcher is Delete-only). Routing kind-filters the cold `Changed` snapshot
    // away, so the Delete-only server must receive NOTHING for `gone` on walk #1.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // Delete the tracked file, then run grep again — the full walk reaps it.
    std::fs::remove_file(&gone)?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // Poll until `gone` is reaped as Deleted(3) — the positive completion signal
    // from walk #2. Walk #1 precedes walk #2 on the FIFO mockls pipe, so once the
    // Deleted(3) is logged, anything walk #1 routed for `gone` is already written
    // too. That lets the absence below (walk #1 routed no Create/Change for `gone`)
    // be checked over the SAME snapshot — no fixed-sleep absence guess.
    let changes = wait_for_change(&log_path, &gone_uri, 3);
    let log = read_merged_log(&log_path);

    // The Delete-only watcher's baselined file is reaped as Deleted(3).
    assert!(
        changes.iter().any(|(u, t)| *u == gone_uri && *t == 3),
        "a Delete-only watcher's baselined file must be reaped as Deleted(3) on \
         the full walk after deletion. changes={changes:?}, log:\n{log}"
    );
    // The Delete-only watcher received NOTHING for `gone` while it was present:
    // its ONLY entry for `gone` is the Deleted(3) — never a Create(1)/Change(2)
    // from the cold walk #1 (kind-filtered away).
    assert!(
        !changes.iter().any(|(u, t)| *u == gone_uri && *t != 3),
        "a Delete-only watcher must receive NOTHING for a present file (no \
         Create/Change from the cold walk) — only its later Deleted(3). \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

// ── glob — scoped, add/update only (ticket 04) ───────────────────────────

/// glob is a `Scoped` walk: it adds/updates within its pattern but NEVER reaps.
/// A file deleted **outside** the globbed directory must not be reaped (the
/// scoped walk can't assert it's gone); a file **inside** the pattern that
/// changed is routed.
#[test]
fn glob_scoped_adds_but_never_reaps() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    // `sub/` holds the globbed file; `outside.*` lives at the root, out of the
    // `sub/` glob pattern.
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub)?;
    let inside = sub.join(format!("inside.{MOCK_LANG_A}"));
    let outside = dir.path().join(format!("outside.{MOCK_LANG_A}"));
    std::fs::write(&inside, "fn a() {}\n")?;
    std::fs::write(&outside, "fn b() {}\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First grep over the whole tree seeds the baseline with BOTH files (so the
    // outside file IS in the baseline, and a reaping walk would catch its
    // deletion — but glob must not reap). Baseline recorded synchronously.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "fn" }))?;

    // Delete the OUTSIDE file and change the INSIDE file (mtime advance gated).
    std::fs::remove_file(&outside)?;
    rewrite_advancing_mtime(&inside, "fn a() {}\nfn c() {}\n")?;

    // glob only `sub/` — a scoped walk of that pattern.
    let sub_str = sub.to_str().context("sub path")?;
    let _ = bridge.call_tool_text("glob", &json!({ "paths": [sub_str] }))?;

    let outside_uri = format!("file://{}/outside.{MOCK_LANG_A}", dir.path().display());
    let inside_uri = format!("file://{}/sub/inside.{MOCK_LANG_A}", dir.path().display());

    // Poll the live log until the in-scope `inside` is routed Changed(2) (the
    // positive completion signal that the scoped glob walk ran and flushed), then
    // assert in that SAME snapshot that the out-of-scope deleted `outside` is NOT
    // reaped. No fixed sleep.
    let changes = wait_for_change(&log_path, &inside_uri, 2);
    let log = read_merged_log(&log_path);

    // The changed inside file must be routed as Changed(2).
    assert!(
        changes.iter().any(|(u, t)| *u == inside_uri && *t == 2),
        "the changed file inside the glob pattern must be routed as \
         Changed(2). changes={changes:?}, log:\n{log}"
    );
    // The deleted outside file must NOT be reaped by the scoped glob walk.
    assert!(
        !changes.iter().any(|(u, t)| *u == outside_uri && *t == 3),
        "a scoped glob walk must NOT reap a deletion outside its pattern. \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// glob nudges + settles before querying outlines: an externally-changed file
/// in the pattern is routed (the scoped nudge fired) and glob returns the fresh
/// outline.
#[test]
fn glob_routes_changed_then_queries_outline() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let file = dir.path().join(format!("outline.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn original() {}\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First glob seeds the baseline (recorded synchronously inside the call).
    let file_str = file.to_str().context("file path")?;
    let _ = bridge.call_tool_text("glob", &json!({ "paths": [file_str] }))?;

    // Externally change the file in the pattern (mtime advance gated).
    rewrite_advancing_mtime(&file, "fn original() {}\nfn added() {}\n")?;

    // Second glob: scoped nudge fires for the changed file, then queries outline.
    let _ = bridge.call_tool_text("glob", &json!({ "paths": [file_str] }))?;

    let file_uri = format!("file://{}/outline.{MOCK_LANG_A}", dir.path().display());

    // Poll the live log until the externally-changed file is routed Changed(2)
    // (the positive completion signal), then assert. No fixed sleep.
    let changes = wait_for_change(&log_path, &file_uri, 2);
    let log = read_merged_log(&log_path);
    assert!(
        changes.iter().any(|(u, t)| *u == file_uri && *t == 2),
        "glob's scoped walk must route the externally-changed file in its \
         pattern as Changed(2). changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// The raw-vs-canonical seam guard (misc 193, mirroring grep's b9145d5 cwd
/// guard): a glob whose raw-absolute pattern path enters under a **symlinked**
/// prefix must resolve its root and enrich the file, not fall back to the
/// `(no LSP)` / `no outline` degradation.
///
/// The root the daemon tracks is canonical (`CATENARY_ROOTS` is canonicalized),
/// but the host passes its raw pattern spelling. On a symlinked-tempdir host
/// (macOS `$TMPDIR` → `/private/var/…`, or any symlinked prefix — Linux
/// reproduces via `TMPDIR` pointed at a symlink, misc 164's recipe) the two
/// spellings differ. A raw pattern base makes glob's expansion walk emit
/// raw-spelled paths that fail `resolve_root`'s canonical prefix check: the file
/// then renders `(no LSP)` under its raw spelling and its
/// `ensure_and_wait_for_paths` / `ensure_symbols` never spawn a server for it,
/// so the outline is lost (`no outline`). Canonicalizing at glob's ingestion
/// seam keeps the walk, `resolve_root`, the display `strip_prefix`, and the
/// enrichment all canonical-to-canonical, so the file resolves to its root and
/// its symbols are queried.
///
/// This test deliberately uses a **raw** `tempfile::tempdir()` (NOT
/// `canonical_tempdir`) — the raw tempdir IS the regression guard. On a
/// non-symlinked host it is identical to the canonical-tempdir variant
/// (canonicalizing an already-canonical base is a no-op), so it is a permanent
/// guard, not a symlink-only branch. `mockls` extracts a `documentSymbol` from
/// the `fn <name>` line, so a served file's outline names that symbol.
#[test]
fn glob_enriches_file_under_symlinked_pattern_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("outline.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn original() {}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Glob the file by its raw (symlinked-prefix) absolute path — the spelling
    // the host passes. It must resolve to its canonical root and enrich.
    let file_str = file.to_str().context("file path")?;
    let out = bridge.call_tool_text("glob", &json!({ "paths": [file_str] }))?;

    // resolve_root must succeed for the in-root file: no `(no LSP)` degradation.
    assert!(
        !out.contains("no LSP"),
        "glob of an in-root file under a symlinked prefix must resolve its root, \
         not render `(no LSP)`. Pre-fix the raw path fails `resolve_root`. out:\n{out}"
    );
    // The file is served: `resolve_root` succeeded, the server was ensured for
    // the canonical path, and `documentSymbol` returned the `fn original` symbol.
    assert!(
        out.contains("original") && !out.contains("no outline"),
        "glob under a symlinked prefix must enrich the file (outline names \
         `original`), not degrade to `no outline`. Pre-fix the raw path is never \
         ensured on the server. out:\n{out}"
    );
    // The rendered path is canonical (the daemon's spelling), matching the
    // canonical root — not the raw `.tlink`-style symlinked prefix.
    let canonical_file = file.canonicalize().context("canonicalize file")?;
    assert!(
        out.contains(&canonical_file.to_string_lossy().into_owned()),
        "glob must render the file at its canonical path. out:\n{out}"
    );
    Ok(())
}
