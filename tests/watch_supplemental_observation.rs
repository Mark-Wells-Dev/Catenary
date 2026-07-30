// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![cfg(unix)]
#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::similar_names,
    reason = "parallel server-A/server-B bindings read clearly with the _a/_b suffix"
)]
//! Bug 143 — the supplemental watch-observation leg.
//!
//! Catenary runs no OS watcher: every observation set that feeds
//! `didChangeWatchedFiles` comes from one of Catenary's own walks, and each is
//! built in **search posture** (`git_ignore(true).hidden(true)`). Registrations
//! were consumed only as a *filter* over what those walks happened to see, so
//! three path classes were structurally unobservable however correctly delivery
//! fanned out — a `**/.lattice.toml` watcher registered by Lattice took 1,160
//! `.md` events and zero config events in one session, freezing the server's
//! config at spawn for the whole session.
//!
//! The fix serves the union of registered globs with the search filters OFF.
//! These guards drive real registrations through `mockls --register-file-watchers`
//! and assert what each server received via `--notification-log`:
//!
//! - `bug143_dotfile_marker_watch_delivers_its_change` — the incident shape.
//! - `bug143_gitignored_literal_watch_delivers_its_change` — the rust-analyzer /
//!   clangd build-artifact shape.
//! - `bug143_base_anchored_watch_reaches_into_a_hidden_tree` — the `baseUri` `OUT_DIR`
//!   shape, the one leg that recurses.
//! - `bug143_supplemental_observation_respects_the_kind_mask` — routing stays
//!   kind-gated for supplementally-observed paths, deletions included.
//! - `bug143_edited_file_still_reaches_a_watcher_that_is_not_diagnosing_it` — the
//!   diagnostics edited-set exclusion is per server, so the watcher that is
//!   never sent the document is not starved (the incident's second half:
//!   taplo diagnoses `.lattice.toml`, lattice watches it).

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::json;

use common::{
    BridgeProcess, mockls_lsp_arg, poll_log_until, read_merged_log, rewrite_advancing_mtime,
    wait_for_change,
};

/// Blessed persona server keys (diagnostics-debt 04c), matching `changed_set.rs`.
const MOCK_LANG_A: &str = "mockls-event";
/// Second mock language, for the tests that run two servers at once.
const MOCK_LANG_B: &str = "mockls-declared";

/// Seeds a probe-bait file for `lang` in `dir` (bug 133 lean 2). The eager
/// health probe opens the sorted-first matching file at spawn and leaves it HELD
/// OPEN, and an open document routes external changes as the didChange full-text
/// relay rather than `didChangeWatchedFiles`. These tests assert the
/// watched-files leg, so the bait soaks up the probe.
fn seed_probe_bait(dir: &Path, lang: &str) -> Result<()> {
    std::fs::write(dir.join(format!("_probe_bait.{lang}")), "bait\n")?;
    Ok(())
}

/// Runs a git command in `cwd`, asserting success (uses the test's real env, so
/// git is on PATH).
fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

/// Counts how many times `uri` was routed with wire `FileChangeType` `typ`.
fn count_changes(changes: &[(String, u64)], uri: &str, typ: u64) -> usize {
    changes
        .iter()
        .filter(|(u, t)| u == uri && *t == typ)
        .count()
}

/// A registered dotfile watcher must see its file's on-disk change.
///
/// The bug-143 incident verbatim: Lattice registers `**/.lattice.toml`, the file
/// is edited mid-session, and the running server never hears about it because
/// every walk that generates observations skips hidden paths. Here the marker is
/// present before spawn, so the cold walk must snapshot it (`Changed(2)` #1) and
/// the walk after the rewrite must route the change (`Changed(2)` #2). Pre-fix
/// the marker produced **zero** events, cold or otherwise.
#[test]
fn bug143_dotfile_marker_watch_delivers_its_change() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    seed_probe_bait(dir.path(), MOCK_LANG_A)?;
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "needle\n")?;

    // Hidden, and NOT of the mock language: the marker is watched by this server
    // but owned by nobody — exactly the `.lattice.toml`/taplo split.
    let marker = dir.path().join(".marker.toml");
    std::fs::write(&marker, "artifacts = []\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/.marker.toml \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let marker_uri = format!("file://{}/.marker.toml", dir.path().display());

    // Walk #1: the cold snapshot. The marker is hidden, so only the supplemental
    // leg can put it in the baseline.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    let cold = wait_for_change(&log_path, &marker_uri, 2);
    let cold_log = read_merged_log(&log_path);
    assert!(
        count_changes(&cold, &marker_uri, 2) >= 1,
        "the cold walk must snapshot the registered dotfile — the search-posture \
         walk cannot see it, so this is the supplemental leg's work. \
         changes={cold:?}, log:\n{cold_log}"
    );

    // Walk #2: the on-disk change must reach the server.
    rewrite_advancing_mtime(&marker, "artifacts = [\"SUBAGENTS.md\"]\n")?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let changes = poll_log_until(&log_path, |c| count_changes(c, &marker_uri, 2) >= 2);
    let log = read_merged_log(&log_path);
    assert!(
        count_changes(&changes, &marker_uri, 2) >= 2,
        "the edit to the registered dotfile must be routed as Changed(2) on the \
         next nudge (cold snapshot + the edit = 2). changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// A registered literal path inside a gitignored tree must be observed.
///
/// The rust-analyzer / clangd shape: build artifacts (`OUT_DIR` products,
/// `compile_commands.json`) live in gitignored directories the search-posture
/// walk refuses to enter, so a watcher on them never fired. A literal pattern
/// needs no walk at all — it is one stat with the filters off.
#[test]
fn bug143_gitignored_literal_watch_delivers_its_change() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    seed_probe_bait(dir.path(), MOCK_LANG_A)?;
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "needle\n")?;

    // `.gitignore` is only honored inside a git repo (`ignore`'s `require_git`),
    // so the ignored tree must be a real one for this guard to be load-bearing.
    run_git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join(".gitignore"), "generated/\n")?;
    let generated = dir.path().join("generated");
    std::fs::create_dir(&generated)?;
    let artifact = generated.join("compile_commands.json");
    std::fs::write(&artifact, "[]\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob generated/compile_commands.json \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let artifact_uri = format!(
        "file://{}/generated/compile_commands.json",
        dir.path().display()
    );

    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    let cold = wait_for_change(&log_path, &artifact_uri, 2);
    let cold_log = read_merged_log(&log_path);
    assert!(
        count_changes(&cold, &artifact_uri, 2) >= 1,
        "the cold walk must snapshot the registered gitignored path. \
         changes={cold:?}, log:\n{cold_log}"
    );

    rewrite_advancing_mtime(&artifact, "[{}]\n")?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let changes = poll_log_until(&log_path, |c| count_changes(c, &artifact_uri, 2) >= 2);
    let log = read_merged_log(&log_path);
    assert!(
        count_changes(&changes, &artifact_uri, 2) >= 2,
        "the rebuild of the gitignored artifact must be routed as Changed(2) on \
         the next nudge. changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// A `baseUri`-anchored watcher must reach into the tree it names.
///
/// rust-analyzer registers `{ baseUri: <OUT_DIR>, pattern: "**/*" }` per build
/// script. The pattern genuinely needs recursion, and the base is the bound: a
/// de-filtered walk of that one server-named directory, never of the root.
#[test]
fn bug143_base_anchored_watch_reaches_into_a_hidden_tree() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    seed_probe_bait(dir.path(), MOCK_LANG_A)?;
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "needle\n")?;

    // A hidden output tree: the search-posture walk never descends it.
    let out_dir = dir.path().join(".out");
    std::fs::create_dir(&out_dir)?;
    let nested = out_dir.join("nested");
    std::fs::create_dir(&nested)?;
    let generated = nested.join("generated.rs");
    std::fs::write(&generated, "// generated\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let base_arg = out_dir.to_str().context("base path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-base {base_arg} --watcher-glob **/* \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let generated_uri = format!("file://{}/.out/nested/generated.rs", dir.path().display());

    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    let cold = wait_for_change(&log_path, &generated_uri, 2);
    let cold_log = read_merged_log(&log_path);
    assert!(
        count_changes(&cold, &generated_uri, 2) >= 1,
        "the cold walk must snapshot the base-anchored tree's contents, nested \
         directories included. changes={cold:?}, log:\n{cold_log}"
    );

    rewrite_advancing_mtime(&generated, "// regenerated\n")?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let changes = poll_log_until(&log_path, |c| count_changes(c, &generated_uri, 2) >= 2);
    let log = read_merged_log(&log_path);
    assert!(
        count_changes(&changes, &generated_uri, 2) >= 2,
        "the regenerated artifact under the watcher's baseUri must be routed as \
         Changed(2) on the next nudge. changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// Supplemental observation widens what is *seen*, never what is *routed*.
///
/// Two servers register the same dotfile marker; A takes all kinds, B takes
/// Delete only (kind 4). B must be sent nothing for the cold snapshot or the
/// edit — and must still receive the `Deleted(3)` when the marker is removed,
/// which also pins that a supplementally-observed path is a full baseline member
/// (a Delete-only watcher's files must enter the baseline while present, or the
/// reaping sweep could never emit their deletion).
#[test]
fn bug143_supplemental_observation_respects_the_kind_mask() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_all = dir.path().join("notifications_all.jsonl");
    let log_delete = dir.path().join("notifications_delete.jsonl");
    seed_probe_bait(dir.path(), MOCK_LANG_A)?;
    seed_probe_bait(dir.path(), MOCK_LANG_B)?;
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "needle\n")?;
    // Spawns server B (only languages with matching files are spawned), so its
    // kind-mask suppression below is a real filter, not an empty log.
    std::fs::write(dir.path().join(format!("b.{MOCK_LANG_B}")), "needle\n")?;

    let marker = dir.path().join(".marker.toml");
    std::fs::write(&marker, "artifacts = []\n")?;

    let log_all_arg = log_all.to_str().context("all log path")?;
    let log_delete_arg = log_delete.to_str().context("delete log path")?;
    let lsp_all = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/.marker.toml \
             --watcher-kind 7 --notification-log {log_all_arg}"
        ),
    );
    // kind 4 — Delete only, no Create/Change bit.
    let lsp_delete = mockls_lsp_arg(
        MOCK_LANG_B,
        &format!(
            "--register-file-watchers --watcher-glob **/.marker.toml \
             --watcher-kind 4 --notification-log {log_delete_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_all, &lsp_delete], root)?;
    bridge.initialize()?;

    let marker_uri = format!("file://{}/.marker.toml", dir.path().display());

    // Walk #1: cold snapshot. A (kind 7) receives it; B (kind 4) must not.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    let all_cold = wait_for_change(&log_all, &marker_uri, 2);
    let all_cold_log = read_merged_log(&log_all);
    assert!(
        count_changes(&all_cold, &marker_uri, 2) >= 1,
        "the all-kinds server must receive the supplementally-observed marker. \
         changes={all_cold:?}, log:\n{all_cold_log}"
    );

    // Walk #2: the marker is gone, so the reaping full walk routes Deleted(3) —
    // B's positive anchor, and proof the marker really was in the baseline.
    std::fs::remove_file(&marker)?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let delete_changes = wait_for_change(&log_delete, &marker_uri, 3);
    let delete_log = read_merged_log(&log_delete);
    assert!(
        delete_changes
            .iter()
            .any(|(u, t)| *u == marker_uri && *t == 3),
        "the Delete-only server must receive the marker's Deleted(3) — a \
         supplementally-observed path is a full baseline member. \
         changes={delete_changes:?}, log:\n{delete_log}"
    );
    // The negative, asserted over the SAME snapshot as the anchor above: nothing
    // but the delete may have reached the Delete-only server.
    assert_eq!(
        count_changes(&delete_changes, &marker_uri, 2),
        0,
        "a Delete-only watcher must not receive Changed(2) for a \
         supplementally-observed path — widening what we observe must not widen \
         what we route. changes={delete_changes:?}, log:\n{delete_log}"
    );
    assert_eq!(
        count_changes(&delete_changes, &marker_uri, 1),
        0,
        "a Delete-only watcher must not receive Created(1) either. \
         changes={delete_changes:?}, log:\n{delete_log}"
    );
    Ok(())
}

/// The diagnostics edited-set exclusion is **per server**.
///
/// An edited file rides document-sync (didOpen/didSave) and is therefore kept
/// out of the watched-files emission — but only for the servers that are
/// actually sent the document. In the bug-143 incident `.lattice.toml` was
/// edited in the same batch as an `.md` file: taplo diagnosed it (so it entered
/// the edited set) while lattice — which registered the watcher — was never sent
/// the document. Excluding it for everyone starved lattice permanently: the
/// per-root baseline advances for the whole root, so no later walk has a delta
/// left to re-emit.
///
/// Here server A owns the language and diagnoses the edited file; server B only
/// watches it. B must receive the change.
#[test]
fn bug143_edited_file_still_reaches_a_watcher_that_is_not_diagnosing_it() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_a = dir.path().join("notifications_a.jsonl");
    let log_b = dir.path().join("notifications_b.jsonl");
    seed_probe_bait(dir.path(), MOCK_LANG_A)?;
    seed_probe_bait(dir.path(), MOCK_LANG_B)?;
    let edited = dir.path().join(format!("edited.{MOCK_LANG_A}"));
    std::fs::write(&edited, "one\n")?;
    // Spawns server B so it is a live covering watcher.
    std::fs::write(dir.path().join(format!("b.{MOCK_LANG_B}")), "one\n")?;

    let log_a_arg = log_a.to_str().context("log a path")?;
    let log_b_arg = log_b.to_str().context("log b path")?;
    // Server A owns MOCK_LANG_A, so it diagnoses (and document-syncs) the edited
    // file. `--advertise-save` lets it ride didOpen/didSave.
    let lsp_a = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --advertise-save --notification-log {log_a_arg}"
        ),
    );
    // Server B watches the same glob but owns a different language, so it is
    // never sent the document.
    let lsp_b = mockls_lsp_arg(
        MOCK_LANG_B,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_A} \
             --watcher-kind 7 --notification-log {log_b_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_a, &lsp_b], root)?;
    bridge.initialize()?;

    let edited_str = edited.to_str().context("edited path")?;
    let edited_uri = format!("file://{}/edited.{MOCK_LANG_A}", dir.path().display());

    // Round #1 seeds the per-root baseline (both servers snapshot the file).
    // Anchored on B's own receipt so the recorded count is a real floor, not a
    // not-yet-flushed zero that the final assertion could clear vacuously.
    let _ = bridge.call_diagnostics(edited_str)?;
    let cold = wait_for_change(&log_b, &edited_uri, 2);
    let floor = count_changes(&cold, &edited_uri, 2);

    // The edit + the diagnose round that carries it: `edited` is in the batch, so
    // it is document-synced to A and excluded there — B must still be told.
    rewrite_advancing_mtime(&edited, "one\ntwo\n")?;
    let _ = bridge.call_diagnostics(edited_str)?;

    let changes = poll_log_until(&log_b, |c| count_changes(c, &edited_uri, 2) > floor);
    let log = read_merged_log(&log_b);
    assert!(
        count_changes(&changes, &edited_uri, 2) > floor,
        "a server that WATCHES the edited file but is never sent the document \
         must receive its change as Changed(2) — the edited-set exclusion covers \
         only the servers that ride document-sync. floor={floor}, \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}
