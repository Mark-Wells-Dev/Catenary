// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![cfg(unix)]
#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! WS31-review ticket R1 — reaping must never run over a partial observation set.
//!
//! Two RED tests demonstrating bugs C1 and H1 (both in
//! `src/bridge/grep_server.rs`'s ticket-04 reaping path). Both are
//! `#[ignore]`d so the default gate stays green; un-ignore them in the fix.
//!
//! - **C1** (`ws31_review_r1_scoped_grep_no_spurious_delete`): a path-scoped
//!   enriched grep takes `WalkBreadth::Full` whenever the root has covering
//!   watchers, so its partial observation set (only the scoped subtree) is fed
//!   to `diff_update_and_reap`, which reaps every baselined file outside the
//!   scope as `Deleted(3)`.
//! - **H1** (`ws31_review_r1_incomplete_observation_not_reaped`): a present file
//!   passing `is_file` via cached `d_type` whose *fresh* `metadata()` stat then
//!   races (here, EACCES from a parent with no execute bit) is omitted from the
//!   observation set with no retry / no prior-mtime fallback, so a full walk
//!   false-reaps it as `Deleted(3)`.

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, mockls_lsp_arg};

const MOCK_LANG: &str = "yX4Za";

/// Reads the notification log and returns every `(uri, type)` pair from every
/// `workspace/didChangeWatchedFiles` notification recorded.
///
/// Copied verbatim from `tests/changed_set.rs` so this RED suite stands alone.
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

/// C1 — a path-scoped enriched grep must not reap files outside its scope.
///
/// The baseline is seeded by a pathless full grep (the harness injects cwd=root,
/// so the walk covers both `a/` and `b/`, baselining `a/match` and `b/keep`).
/// A second grep scoped to `a/` walks only `a/`, so its observation set omits
/// `b/keep`. Because the root has covering watchers, that scoped walk still takes
/// `WalkBreadth::Full` and calls `diff_update_and_reap` with `reap=true` — which
/// reaps `b/keep` (present on disk!) as `Deleted(3)`. RED today.
#[test]
#[ignore = "RED: WS31-review R1; un-ignore in fix"]
fn ws31_review_r1_scoped_grep_no_spurious_delete() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");

    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir(&a)?;
    std::fs::create_dir(&b)?;
    let a_match = a.join(format!("match.{MOCK_LANG}"));
    let b_keep = b.join(format!("keep.{MOCK_LANG}"));
    std::fs::write(&a_match, "needle\n")?;
    std::fs::write(&b_keep, "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 7 (ALL) ⇒ registers Delete, so a spurious `Deleted` IS routed and
    // recorded. A kind without the Delete bit would mask the bug at routing.
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG} \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Seed the per-root baseline with a PATHLESS full grep — the harness injects
    // cwd=root, so ripgrep walks the whole tree and BOTH a/match and b/keep enter
    // the baseline.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    std::thread::sleep(Duration::from_millis(150));

    // Scoped grep: ripgrep walks only a/. Its observation set omits b/keep.
    let a_str = a.to_str().context("a path")?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle", "paths": [a_str] }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let a_match_uri = format!("file://{}/a/match.{MOCK_LANG}", dir.path().display());
    let b_keep_uri = format!("file://{}/b/keep.{MOCK_LANG}", dir.path().display());
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let changes = watched_file_changes(&log);

    // Key assertion (RED today): the scoped grep must NOT reap b/keep — it is
    // present on disk and merely outside the scoped walk's breadth.
    assert!(
        !changes.iter().any(|(u, t)| *u == b_keep_uri && *t == 3),
        "scoped grep must not reap files outside its scope; b/keep is present. \
         changes={changes:?}, log:\n{log}"
    );
    // Companion guard: a/match IS still routed (present, e.g. Changed(2)) so the
    // fix can't trivially pass by making grep stop nudging entirely.
    assert!(
        changes.iter().any(|(u, t)| *u == a_match_uri && *t == 2),
        "the in-scope file a/match must still be routed as Changed(2). \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

/// H1 — a present file whose fresh `metadata()` stat races (EACCES) must not be
/// reaped.
///
/// Deterministic via a directory-permission seam. PRECONDITION: the tempdir must
/// live on a filesystem that populates `d_type` in readdir (e.g. tmpfs, which is
/// where the DEFAULT `tempfile::tempdir()` → `/tmp` lands). On such a filesystem
/// the walker's `is_file` decision is made from the cached `d_type` (no stat at
/// all), so `sub/locked` passes `is_file` even when `sub` has no execute bit —
/// but the SEPARATE fresh `metadata()` two lines later (which needs execute on
/// `sub`) fails with EACCES, and the file is silently dropped from observations
/// → false-reaped on the full walk. On a `DT_UNKNOWN` filesystem the failing
/// stat would instead route through the `is_file` gate (skip-as-non-file), which
/// would mask the bug → false GREEN; that is why we rely on the tmpfs default.
#[test]
#[ignore = "RED: WS31-review R1; un-ignore in fix"]
fn ws31_review_r1_incomplete_observation_not_reaped() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Root guard: root ignores the missing execute bit, so the EACCES seam would
    // never fire → false GREEN. No `geteuid` in std and no libc/nix/rustix dep is
    // available, so detect root *behaviorally*: probe the seam itself in a scratch
    // tempdir. If a fresh stat under a 0o400 dir succeeds, we are effectively root
    // (or on a permission-ignoring FS) and must skip — this also guards the
    // permission-ignoring-FS case the ticket warns about.
    // When the seam is ineffective (root, or a permission-ignoring filesystem)
    // the EACCES path cannot be exercised, so skip rather than false-GREEN.
    if seam_is_ineffective() {
        return Ok(());
    }

    // DEFAULT tempdir() → /tmp (tmpfs), which populates d_type. See precondition.
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");

    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub)?;
    let locked = sub.join(format!("locked.{MOCK_LANG}"));
    // A root-level keep so the post-flap grep still matches a file and runs the
    // full walk (which fires the reap sweep over the baseline).
    let keep = dir.path().join(format!("keep.{MOCK_LANG}"));
    std::fs::write(&locked, "needle\n")?;
    std::fs::write(&keep, "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 7 (ALL) ⇒ registers Delete, so a spurious `Deleted` IS routed/recorded.
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG} \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Seed the baseline with a pathless full grep (normal perms) → both
    // sub/locked and keep baselined.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    std::thread::sleep(Duration::from_millis(150));

    // Strip execute (search) from sub: readdir still works (read granted) so
    // sub/locked is enumerated with cached d_type and passes `is_file`, but a
    // fresh metadata() stat on it needs execute on sub → EACCES → omitted from
    // observations → false-reaped on the full walk. sub/locked is still on disk.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o400))?;

    // Pathless full grep again — covers the whole tree; reap sweep fires.
    let grep_result = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }));

    // RESTORE execute immediately, in the test BODY, before drop(bridge): the
    // tempdir's Drop must recurse into sub to clean up, which needs execute.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700))?;
    grep_result?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let locked_uri = format!("file://{}/sub/locked.{MOCK_LANG}", dir.path().display());
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let changes = watched_file_changes(&log);

    // Key assertion (RED today): a present file whose fresh metadata() raced
    // (EACCES) must not be reaped.
    assert!(
        !changes.iter().any(|(u, t)| *u == locked_uri && *t == 3),
        "a present file whose fresh metadata() raced (EACCES) must not be reaped. \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

// ── R2 (H2) — traversed symlink-to-file is skipped by default ─────────────

/// H2 — a traversed symlink-to-a-file is skipped (not searched), while real
/// files are searched. GREEN guard, NOT `#[ignore]`d.
///
/// WS31 ticket 01 switched the walker's file decision from `path.is_file()`
/// (follows symlinks, fresh stat) to `entry.file_type()` (cached `d_type`).
/// Because `follow_links` is never set on the `WalkBuilder`, a traversed symlink
/// entry reports its OWN type (`is_symlink()==true`, `is_file()==false`) and is
/// dropped at `grep_server.rs`'s `debug!("grep: skipping non-file entry")`. The
/// review RESOLVED this as ripgrep-parity: the default-skip is CORRECT. An
/// in-tree symlink target's content is still found via its real path, and
/// following would produce DUPLICATE matches under both the link and the target;
/// the only gap (a target outside the walked set) becomes the opt-in
/// `--follow-links` in WS31-review ticket 07. This test pins that parity and
/// guards against an accidental re-regression to following. The `d_type`
/// precondition the R1/H1 test relies on does NOT matter here — that gates only
/// the unreachable walker *retry* branch, not this default-skip; `file_type()`
/// returns the symlink's own type on every filesystem.
#[test]
fn ws31_review_r2_traversed_symlink_file_is_skipped() -> Result<()> {
    use std::os::unix::fs;

    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");

    // A real file at the root carrying the needle — ensures grep returns matches
    // and runs the full directory walk (so the symlink IS traversed/decided).
    let real_hit = dir.path().join(format!("real_hit.{MOCK_LANG}"));
    std::fs::write(&real_hit, "needle\n")?;

    // In sub/: a real target carrying the needle, plus a RELATIVE symlink to the
    // sibling target. The walker traverses the link entry; with no follow_links
    // its own type is `symlink` (not `file`) → skipped. The target is still found
    // directly under `sub/target`.
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub)?;
    let target = sub.join(format!("target.{MOCK_LANG}"));
    std::fs::write(&target, "needle\n")?;
    let link = sub.join(format!("link.{MOCK_LANG}"));
    fs::symlink(format!("target.{MOCK_LANG}"), &link)?;

    let log_arg = log_path.to_str().context("log path")?;
    // A watcher-registering mockls so grep runs its full enriched pipeline (same
    // minimal spawn as the changed_set.rs suite). H2 asserts on grep's own RESULT
    // TEXT, not on the notification log — the watcher is just to exercise the
    // full walk, not because the assertion reads notifications.
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG} \
             --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Pathless grep → the harness injects cwd=root → directory traversal. Result
    // paths are rendered cwd-relative (e.g. `real_hit.<LANG>`, `sub/target.<LANG>`).
    let out = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(150));

    // Real files are searched and their matches appear.
    assert!(
        out.contains(&format!("real_hit.{MOCK_LANG}")),
        "the real root file must be searched and listed. out:\n{out}"
    );
    assert!(
        out.contains(&format!("target.{MOCK_LANG}")),
        "the real symlink TARGET (found directly under sub/) must be listed. \
         out:\n{out}"
    );
    // Key guard (GREEN today): the traversed symlink is skipped by default, so
    // `link.<LANG>` must NOT appear as a result path. (`link.<LANG>` is not a
    // substring of `target.<LANG>`, so this can't false-pass on the target hit.)
    // Pins ripgrep-parity default-skip (WS31-review H2); following is opt-in via
    // ticket 07. A re-regression to follow_links would surface `link.<LANG>` here.
    assert!(
        !out.contains(&format!("link.{MOCK_LANG}")),
        "a traversed symlink-to-file must be SKIPPED by default (ripgrep-parity, \
         WS31-review H2); `link.{MOCK_LANG}` must not be listed. out:\n{out}"
    );
    Ok(())
}

// ── R2 (L4) — live-retry transient-miss recovery: NOT WRITTEN ─────────────
//
// L4 asks for a test proving the live retry helpers actually RETRY — a transient
// miss-then-hit that recovers, such that the test would FAIL if
// `STAT_RETRY_ATTEMPTS` were 1. This was NOT written: it cannot be done
// deterministically and test-only without a production change.
//
// Both live helpers —
//   `session.rs::path_exists_with_retry`        (uses `symlink_metadata`)
//   `file_tools.rs::path_is_file_or_symlink_with_retry` (uses `is_file`/`is_symlink`)
// — and the (dead, walker-only) `grep_server.rs::stat_is_file_with_retry` loop
// `0..STAT_RETRY_ATTEMPTS` with NO sleep and NO yield between attempts (their
// doc-comments are explicit: "the rename window is sub-millisecond" → tight
// back-to-back syscalls). With no inter-attempt window there is no deterministic
// way for a background thread to flip a path from miss→hit between attempt 1 and
// a later attempt: any such race is timing-dependent and would be flaky, not a
// GREEN guard. The helpers are private `fn`s taking `&Path` with no injectable
// stat/clock seam. So a retry-count-sensitive test (one that fails at
// `STAT_RETRY_ATTEMPTS == 1`) requires a production change — out of scope for
// this add-tests-only pass. See the scout report for the proposed minimal seam
// and alternatives.

/// Probes whether the directory-permission seam is ineffective in this
/// environment — i.e. a fresh stat of a file under a `0o400` (no-execute) parent
/// still succeeds. That is true when running as root (which bypasses the execute
/// bit) or on a permission-ignoring filesystem; in either case the EACCES path
/// the H1 test relies on cannot be exercised, so the test must skip.
fn seam_is_ineffective() -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Ok(probe) = tempfile::tempdir() else {
        // Can't even make a scratch dir — treat as ineffective and skip.
        return true;
    };
    let pdir = probe.path().join("p");
    if std::fs::create_dir(&pdir).is_err() {
        return true;
    }
    let pfile = pdir.join("f");
    if std::fs::write(&pfile, "x").is_err() {
        return true;
    }
    if std::fs::set_permissions(&pdir, std::fs::Permissions::from_mode(0o400)).is_err() {
        return true;
    }
    let stat_ok = pfile.metadata().is_ok();
    // Restore so the scratch tempdir can be cleaned up on Drop.
    let _ = std::fs::set_permissions(&pdir, std::fs::Permissions::from_mode(0o700));
    // Seam is INeffective when the stat still succeeded despite no execute bit.
    stat_ok
}
