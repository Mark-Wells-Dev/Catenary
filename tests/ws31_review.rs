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
    reason = "parallel server-P/server-S bindings read clearly with the _p/_s suffix"
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

use common::{BridgeProcess, ipc_request, mockls_lsp_arg};

const MOCK_LANG: &str = "yX4Za";
/// Second mock language for the L5 nested-root test (a distinct server).
const MOCK_LANG_S: &str = "z9Qw7";

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

// ── R2 (L4) — live-retry transient-miss recovery (landed in Phase B) ──────
//
// The L4 guard `ws31_review_r2_live_retry_recovers_transient_miss` lives next to
// the helpers it covers, in the `#[cfg(test)]` modules of
// `src/bridge/session.rs` (`path_exists_with_retry`) and
// `src/bridge/file_tools.rs` (`path_is_file_or_symlink_with_retry`). Phase B gave
// each helper a `#[cfg(test)]` injectable-probe seam (an inner `_with(path,
// attempts, probe)` fn); the guard injects a fail-then-succeed probe to prove the
// retry loop recovers, and asserts it fails at `attempts == 1` (retry-count-
// sensitive). The walker's dead `stat_is_file_with_retry` was removed (M3).

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

// ── R3 (M1) — daemon-lived ResultCache not cleared on root removal ─────────

/// M1 — the per-`GrepServer` `ResultCache` is NOT evicted when a root leaves
/// the tracked set, so a repeated multi-page enriched grep serves the cached
/// page — with its `calls:` enrichment — for a path that is now untracked.
///
/// `Session::sync_roots` evicts the `SymbolIndex` on root removal (ticket 05)
/// but leaves `grep.cache` untouched. The cache short-circuit
/// (`grep_server.rs::execute`) runs BEFORE root resolution, and on root removal
/// `root_generation` reverts to 0 (`filesystem_manager.rs`). For a read-only
/// session (generation stays 0) over unchanged files, the cached page's
/// generation/witness guards still validate, so the stale page is served.
///
/// Single-page results skip caching (`result_cache.rs::put`: `total_pages <= 1`),
/// which is why the sibling `cache_eviction.rs` tests (whose fixtures render one
/// page) never reach the cache and so miss this bug. This test deliberately
/// builds a MULTI-page enriched fixture so the warm grep IS cached, then removes
/// the root and re-greps the identical page-1 query.
///
/// Build/warm/remove/re-grep shape is modeled on
/// `cache_eviction.rs::enrichment_evicted_on_root_removal`; the caller/callee
/// fixture is modeled on its `caller_callee` body. To keep each caller its own
/// name-group with exactly one outgoing edge, names are 4-digit zero-padded so
/// no callee name is a substring of another (`callee_0001` does not contain
/// `callee_0000`).
#[test]
fn ws31_review_r3_resultcache_not_served_for_untracked_root() -> Result<()> {
    use std::fmt::Write as _;

    // Number of caller/callee pairs in the fixture. Sized so the rendered
    // enriched page-1 output exceeds the 4000-char budget (multi-page ⇒ cached);
    // single-page results skip caching, which is why a smaller fixture would not
    // exercise the bug. Empirically ~45 pairs cross the budget; 150 is a margin.
    const PAIRS: usize = 150;

    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // The warmed root is a sibling directory added at runtime. Canonicalize so
    // it matches the canonical form `roots-add` stores and `roots-ls` reports.
    let sibling = tempfile::tempdir()?;
    let sibling_path = sibling.path().canonicalize()?;
    let sibling_str = sibling_path.to_str().context("sibling path")?;

    // ONE file with many padded caller/callee pairs so the rendered enriched
    // grep output exceeds the 4000-char page budget (multi-page ⇒ cached).
    // Each caller's body names exactly its own callee; 4-digit padding keeps
    // every name a distinct group with a single outgoing edge.
    let mut body = String::new();
    for i in 0..PAIRS {
        let _ = write!(
            body,
            "fn callee_{i:04}\nfn caller_{i:04} {{\ncallee_{i:04}\n}}\n"
        );
    }
    let file = sibling.path().join(format!("warm.{MOCK_LANG}"));
    std::fs::write(&file, &body)?;

    // `--scan-roots` so documentSymbol + outgoingCalls resolve from the startup
    // index (enrichment works without per-file didOpen).
    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], base_str)?;
    bridge.initialize()?;

    let socket = bridge.wait_for_ipc_socket()?;

    // Add the sibling root via the hook contributor (`catenary roots add`),
    // then wait for it to appear as tracked.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": sibling_str }),
    )?;
    bridge.wait_for_root(sibling_str, Duration::from_secs(5))?;
    // Give the per-root server a moment to spawn before the enriching grep.
    std::thread::sleep(Duration::from_millis(300));

    // Warm the ResultCache (generation stays 0 — no edit/sed/diagnostics, the
    // only generation-bumpers). Page 1 of an enriched grep on the sibling root.
    let warm = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "caller_", "directory": sibling_str }),
    )?;
    // Preconditions — these MUST pass; they prove the setup is genuinely
    // multi-page AND enriched (i.e. actually cached). If either fails, the
    // SETUP is wrong (bump PAIRS / fix enrichment), not the bug.
    assert!(
        warm.starts_with("[page 1/"),
        "warming grep must be multi-page (⇒ cached); bump PAIRS until it is. got:\n{warm}"
    );
    assert!(
        warm.contains("calls:"),
        "warming grep must be enriched (calls: section), got:\n{warm}"
    );

    // Remove the sibling root via the hook contributor (`catenary roots rm`).
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-rm", "path": sibling_str }),
    )?;
    // Wait until ls-roots no longer reports the sibling as tracked. Do NOT touch
    // any fixture file — witness mtimes must stay unchanged.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ls = ipc_request(&socket, &json!({ "method": "tool/roots-ls" }))?;
        if !ls.contains(sibling_str) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sibling root still tracked after roots-rm: {ls}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Re-grep the identical page-1 query now that the root is untracked.
    let after = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "caller_", "directory": sibling_str }),
    )?;

    // RED (the bug): the now-untracked root must not serve the cached enriched
    // page. Fails today because the ResultCache short-circuit runs before root
    // resolution and serves the stale page (with its `calls:` section).
    assert!(
        !after.contains("calls:"),
        "untracked root must not serve cached enrichment via the ResultCache \
         after removal; got:\n{after}"
    );
    // Anti-vacuous guard: the raw match must still surface, so the RED above
    // is not a false pass from an empty result.
    assert!(
        after.contains("caller_0000"),
        "re-grep must still surface the raw match; got:\n{after}"
    );

    Ok(())
}

// ── R4 (M2) — eviction witnessed un-spoofably via cold re-query ─────────────

/// Body that makes `mockls --scan-roots` report an outgoing call: a callee
/// defined first, then a caller whose body names the callee. An enriched grep
/// on the caller renders a `calls:` section. Mirrors
/// `cache_eviction.rs::caller_callee`.
fn caller_callee(callee: &str, entry: &str) -> String {
    format!("fn {callee}\nfn {entry} {{\n{callee}\n}}\n")
}

/// Counts the request-log lines whose `method` equals `target`. The mockls
/// `--request-log` appends one `{"method":"..."}` object per handled request.
fn request_method_count(log: &str, target: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.get("method").and_then(Value::as_str) == Some(target))
        .count()
}

/// M2 — witnesses `evict_root` un-spoofably: a re-added root must be a genuine
/// COLD touch, so the daemon re-issues `textDocument/documentSymbol` against the
/// re-spawned server.
///
/// The existing `cache_eviction.rs` tests assert `!contains("calls:")` on a grep
/// against the now-UNTRACKED path — but `enrich_at_position` gates the cache read
/// on `resolve_root(path).is_some()` and the per-root server is shut down on
/// removal, so two independent backstops suppress `calls:` even if `evict_root`
/// were a no-op. This test instead removes AND re-adds the root, then proves the
/// re-grep was cold by reading a request counter on the re-spawned server: a
/// no-op evict would leave the daemon-lived `SymbolIndex` warm, the re-grep would
/// short-circuit on the cache, and the re-spawned server would see ZERO
/// `textDocument/documentSymbol`. Because the re-add `File::create`-truncates the
/// request-log, the window covers only the re-grep's server.
///
/// GREEN today: `evict_root` is correct ⇒ the re-grep is cold ⇒ documentSymbol
/// is re-issued. The mutation check (Phase-A writer stubbed `evict_root` to a
/// no-op) confirms this FAILS when eviction is broken.
#[test]
fn ws31_review_r4_eviction_witnessed_via_request_count() -> Result<()> {
    // The base root keeps the daemon alive across the rm/add cycle.
    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // The warmed root is a sibling directory. Canonicalize so it matches the
    // form `roots-add` stores and `roots-ls` reports.
    let work = tempfile::tempdir()?;
    let work_path = work.path().canonicalize()?;
    let work_str = work_path.to_str().context("work path")?;
    let file = work.path().join(format!("warm.{MOCK_LANG}"));
    std::fs::write(&file, caller_callee("callee_x", "caller_x"))?;

    // The re-spawned server truncates this on `File::create`, so after the re-add
    // it records ONLY the re-grep's requests.
    let req_log_path = work_path.join("requests.jsonl");
    let req_log_arg = req_log_path.to_str().context("request log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!("--scan-roots --request-log {req_log_arg}"),
    );

    let mut bridge = BridgeProcess::spawn(&[&lsp], base_str)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // Add and warm the work root.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    bridge.wait_for_root(work_str, Duration::from_secs(5))?;
    std::thread::sleep(Duration::from_millis(300));

    let warm = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "caller_x", "directory": work_str }),
    )?;
    // Precondition — the setup is genuinely enriched.
    assert!(
        warm.contains("calls:"),
        "warming grep must be enriched (calls: section), got:\n{warm}"
    );

    // Remove the root, then poll until untracked.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-rm", "path": work_str }),
    )?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ls = ipc_request(&socket, &json!({ "method": "tool/roots-ls" }))?;
        if !ls.contains(work_str) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "work root still tracked after roots-rm: {ls}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Re-add the root. The per-root server re-spawns and `File::create`-truncates
    // the request-log, opening a clean window over just the re-grep's server.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    bridge.wait_for_root(work_str, Duration::from_secs(5))?;
    std::thread::sleep(Duration::from_millis(300));

    // Re-grep. With a correct evict, the daemon-lived SymbolIndex was emptied for
    // this root, so this is a genuine cold touch: the daemon re-queries the outline.
    let cold = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "caller_x", "directory": work_str }),
    )?;
    // Anti-vacuous: the re-grep still surfaces the symbol AND re-resolves enrichment.
    assert!(
        cold.contains("caller_x"),
        "re-added root must serve the raw match; got:\n{cold}"
    );
    assert!(
        cold.contains("calls:"),
        "cold first touch after eviction must re-resolve enrichment; got:\n{cold}"
    );

    // Drop the bridge first to shut the server down — its on-exit flush is the
    // durability guarantee the request-log read relies on. The poll below only
    // removes the fixed-time GUESS for when that flush becomes visible to this
    // reader: under heavy parallel load the re-spawned server's requests may not
    // be flushed/visible to the file by a fixed delay, so we re-read + re-parse
    // until both expected counts hold (or the deadline passes). Cadence/cap match
    // the `roots-ls` poll loops above (50 ms between attempts, ~5 s total).
    drop(bridge);
    let req_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut req_log = std::fs::read_to_string(&req_log_path).unwrap_or_default();
    let mut doc_symbol_count = request_method_count(&req_log, "textDocument/documentSymbol");
    let mut outgoing_count = request_method_count(&req_log, "callHierarchy/outgoingCalls");
    while (doc_symbol_count < 1 || outgoing_count < 1) && std::time::Instant::now() < req_deadline {
        std::thread::sleep(Duration::from_millis(50));
        req_log = std::fs::read_to_string(&req_log_path).unwrap_or_default();
        doc_symbol_count = request_method_count(&req_log, "textDocument/documentSymbol");
        outgoing_count = request_method_count(&req_log, "callHierarchy/outgoingCalls");
    }

    // The re-grep was cold ⇒ the daemon re-issued documentSymbol against the
    // re-spawned server. A no-op evict would leave the cache warm ⇒ count 0.
    // A deadline reached with the condition still unmet falls through to FAIL.
    assert!(
        doc_symbol_count >= 1,
        "re-add must be a genuine cold touch — the daemon must re-query the \
         outline (textDocument/documentSymbol) at least once. count={doc_symbol_count}, \
         request log:\n{req_log}"
    );
    // The outgoingCalls re-query is the enrichment edge; its presence corroborates
    // a cold enrichment rather than a stale warm hit.
    assert!(
        outgoing_count >= 1,
        "cold re-enrichment must re-query callHierarchy/outgoingCalls at least \
         once. count={outgoing_count}, request log:\n{req_log}"
    );

    Ok(())
}

// ── R4 (M4) — second walk emits an empty changeset ──────────────────────────

/// M4 — a second walk with no FS change must emit ZERO new
/// `didChangeWatchedFiles` (the bug-38 no-repeat property), positively encoded.
///
/// Strengthens the weak `changed_set.rs::second_walk_sends_only_delta`, which
/// compared only notification COUNTS after a fixed sleep — an under-counted first
/// could equal an under-counted total (false pass), and it never asserted the
/// second changeset was empty. Here the first walk's emitted URIs are captured,
/// the second walk runs with no FS change, and the full log must be IDENTICAL to
/// the first — any re-announced URI is listed. The single append-only server log
/// is order-stable, so equality is exact.
///
/// GREEN today.
#[test]
fn ws31_review_r4_second_walk_emits_empty_changeset() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG}")), "needle\n")?;
    std::fs::write(dir.path().join(format!("b.{MOCK_LANG}")), "other\n")?;

    let log_arg = log_path.to_str().context("log path")?;
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

    // Walk #1 — the cold-start full candidate set.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    std::thread::sleep(Duration::from_millis(150));
    let log_after_first = std::fs::read_to_string(&log_path).unwrap_or_default();
    let after_first = watched_file_changes(&log_after_first);
    // Anti-vacuous: the first walk announced something.
    assert!(
        !after_first.is_empty(),
        "first walk must announce the cold candidate set. log:\n{log_after_first}"
    );

    // Walk #2 — NO FS change.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let full_log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let total = watched_file_changes(&full_log);
    // Positively encodes bug-38 no-repeat: the second walk added zero changes, so
    // the full set equals the first walk's set. Order-stable single-server log.
    assert_eq!(
        total,
        after_first,
        "second walk with no FS change must emit zero new didChangeWatchedFiles; \
         extra: {:?}",
        &total[after_first.len().min(total.len())..]
    );

    Ok(())
}

// ── R4 (L5) — covering_watchers subdir/parent scope prefix matching ─────────

/// L5 — `covering_watchers` includes a subdir-scoped server for a parent walk and
/// excludes a parent-scoped server for a child walk
/// (`scope.root_path().starts_with(root)`).
///
/// Two canonicalized tracked roots `parent` and `parent/sub`, each with its own
/// mockls language/server registering its own glob + its own notification log.
///
/// - **Positive:** a grep over `parent` matches `parent/top.<LANG_S>` (resolving
///   to root `parent`). Server S — scoped to `parent/sub` — is included in that
///   parent walk because `parent/sub` `starts_with` `parent`, so S's log records
///   the `top.<LANG_S>` change.
/// - **Negative:** a SEPARATE grep tightly scoped to `parent/sub` matches
///   `parent/sub/inner.<LANG_P>` (resolving to root `parent/sub`). Server P —
///   scoped to `parent` — is EXCLUDED because `parent` `starts_with` `parent/sub`
///   is false, so P's log records nothing for that walk.
///
/// The two greps are kept separate so a single grep can't span both roots.
/// GREEN today.
#[test]
fn ws31_review_r4_covering_watchers_subdir_scope() -> Result<()> {
    // Parent root and a nested child root. Canonicalize both so the literal
    // `starts_with` prefix relationship holds in the form roots are stored.
    let parent = tempfile::tempdir()?;
    let parent_path = parent.path().canonicalize()?;
    let parent_str = parent_path.to_str().context("parent path")?;

    let sub_path = parent_path.join("sub");
    std::fs::create_dir(&sub_path)?;
    let sub_str = sub_path.to_str().context("sub path")?;

    // Server P (lang = MOCK_LANG) is scoped to `parent`; server S (lang =
    // MOCK_LANG_S) is scoped to `parent/sub`. Two distinct langs ⇒ two servers.
    let log_p = parent_path.join("notifications_p.jsonl");
    let log_s = parent_path.join("notifications_s.jsonl");
    let log_p_arg = log_p.to_str().context("log p path")?;
    let log_s_arg = log_s.to_str().context("log s path")?;

    let lsp_p = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG} \
             --watcher-kind 7 --notification-log {log_p_arg}"
        ),
    );
    let lsp_s = mockls_lsp_arg(
        MOCK_LANG_S,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG_S} \
             --watcher-kind 7 --notification-log {log_s_arg}"
        ),
    );

    // Positive fixture: a file matching S's glob under `parent` (so S's scope
    // `parent/sub` is covered by a `parent` walk), plus a P-glob file so the
    // parent-scoped grep walks `parent`.
    let top_s = parent_path.join(format!("top.{MOCK_LANG_S}"));
    let top_p = parent_path.join(format!("top.{MOCK_LANG}"));
    std::fs::write(&top_s, "needle\n")?;
    std::fs::write(&top_p, "needle\n")?;
    // Negative fixture: a P-glob file under `parent/sub`, matched only by the
    // tightly-scoped child grep.
    let inner_p = sub_path.join(format!("inner.{MOCK_LANG}"));
    std::fs::write(&inner_p, "needle\n")?;

    // Both roots tracked: `parent` and `parent/sub`. Declare them via MCP
    // `roots/list` so they enter the daemon's RootTracker (a plain `initialize`
    // would leave the tracker empty, so `roots-ls` would never report them).
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp_p, &lsp_s], &[parent_str, sub_str])?;
    bridge.initialize_with_roots(&[parent_str, sub_str])?;
    bridge.wait_for_root(parent_str, Duration::from_secs(5))?;
    bridge.wait_for_root(sub_str, Duration::from_secs(5))?;
    std::thread::sleep(Duration::from_millis(300));

    // POSITIVE walk: grep over `parent`. The walk root is `parent`; S (scoped to
    // `parent/sub`) is a covering watcher because `parent/sub`.starts_with(`parent`).
    let _ = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "directory": parent_str }),
    )?;

    // NEGATIVE walk: a SEPARATE grep tightly scoped to `parent/sub`. The walk root
    // is `parent/sub`; P (scoped to `parent`) is EXCLUDED because
    // `parent`.starts_with(`parent/sub`) is false.
    let _ = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "directory": sub_str }),
    )?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let top_s_uri = format!("file://{}/top.{MOCK_LANG_S}", parent_path.display());
    let inner_p_uri = format!("file://{}/sub/inner.{MOCK_LANG}", parent_path.display());

    // Positive: the subdir-scoped server S was included in the parent walk and
    // recorded its globbed file.
    let log_s_text = std::fs::read_to_string(&log_s).unwrap_or_default();
    let s_changes = watched_file_changes(&log_s_text);
    assert!(
        s_changes.iter().any(|(u, _)| *u == top_s_uri),
        "subdir-scoped server (parent/sub) must be INCLUDED in the parent walk \
         (parent/sub starts_with parent) and receive top.{MOCK_LANG_S}. \
         s_changes={s_changes:?}, log:\n{log_s_text}"
    );

    // Negative: the parent-scoped server P must have received NO change for the
    // child-scoped walk's file (parent does not start_with parent/sub).
    let log_p_text = std::fs::read_to_string(&log_p).unwrap_or_default();
    let p_changes = watched_file_changes(&log_p_text);
    assert!(
        !p_changes.iter().any(|(u, _)| *u == inner_p_uri),
        "parent-scoped server (parent) must be EXCLUDED from the child walk \
         (parent does NOT start_with parent/sub) and must not receive \
         sub/inner.{MOCK_LANG}. p_changes={p_changes:?}, log:\n{log_p_text}"
    );

    Ok(())
}

// ── R5 (L6) — an edited-then-deleted file's Deleted is routed, not suppressed ──

/// L6 — a file that is baselined and then deleted from disk is reaped as
/// `Deleted` (wire `FileChangeType` 3) and that Delete is ROUTED, not suppressed
/// — even when the same diagnostics batch carries a live edited sibling.
///
/// The review's L6 premise was that an edited path placed in the diagnostics
/// `exclude` set would suppress a reaped `Deleted` for that path. Verified
/// WRONG: `process_files_batched` builds `exclude` from `canonical_paths`, and a
/// path only enters `canonical_paths` after passing `validate_read`
/// (`diagnostics_server.rs:189`). `validate_read` calls `path.canonicalize()`,
/// which FAILS for a file no longer on disk — so a deleted file never enters
/// `canonical_paths`, never enters `exclude`, and its reaped `Deleted` is routed
/// by `nudge_changed_set` (the `exclude.contains(&change.rel)` guard is false).
///
/// This is a GREEN guard (NOT `#[ignore]`d): it pins that `validate_read` keeps
/// deleted files out of `exclude`, so reaped Deletes for them stay routed. If a
/// future change put raw edited paths into `exclude` BEFORE the existence check
/// (so an edited-then-deleted path landed in `exclude`), this guard would fail —
/// which is exactly the L6 suppression the review feared (and which does NOT
/// occur today).
///
/// Modeled on `changed_set.rs::diagnostics_full_walk_reaps_deletion` /
/// `diagnostics_excludes_edited_set`: the live sibling rides the edited-set (via
/// `call_diagnostics`, which seeds the file through the `pre-tool/editing-state`
/// path) and keeps the root in the diagnostics batch's `roots` set so the
/// stat-walk runs over the root (a deleted-only batch would not walk it). kind 7
/// (ALL) registers Delete, so a routed `Deleted` IS recorded in the log.
#[test]
fn ws31_review_r5_edited_then_deleted_routes_delete() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let live = dir.path().join(format!("live.{MOCK_LANG}"));
    let gone = dir.path().join(format!("gone.{MOCK_LANG}"));
    std::fs::write(&live, "one\n")?;
    std::fs::write(&gone, "one\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 7 (ALL) ⇒ registers Delete, so a routed `Deleted` IS recorded.
    // `--advertise-save` lets the edited live sibling ride document-sync.
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG} \
             --watcher-kind 7 --advertise-save --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Seed the per-root baseline with BOTH files present: the first diagnostics
    // run's full stat-walk observes the whole root, so `live` and `gone` both
    // enter the baseline.
    let _ = bridge.call_diagnostics(live.to_str().context("live path")?)?;
    std::thread::sleep(Duration::from_millis(150));

    // Delete `gone` from disk; `live` stays present and is re-driven through the
    // edited-set below (it keeps the root in the diagnostics batch's `roots` set,
    // so the stat-walk reaps `gone`).
    std::fs::remove_file(&gone)?;

    // Second diagnostics run on `live`: drains the edited-set ⇒ a batch over the
    // root ⇒ the full stat-walk reaps `gone` (baselined but no longer observed).
    let _ = bridge.call_diagnostics(live.to_str().context("live path")?)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let gone_uri = format!("file://{}/gone.{MOCK_LANG}", dir.path().display());
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let changes = watched_file_changes(&log);

    // GREEN today: the deleted file's `Deleted` must be ROUTED. It is not in the
    // exclude set — `validate_read` drops it (canonicalize fails for a missing
    // path) before it could enter `canonical_paths`/`exclude` — so the reaped
    // `Deleted` is delivered (the review's L6 suppression does NOT occur).
    assert!(
        changes.iter().any(|(u, t)| *u == gone_uri && *t == 3),
        "an edited-then-deleted file's Deleted must be routed (it is not in the \
         exclude set — validate_read drops it); got:\n{changes:?}"
    );
    Ok(())
}

// ── C1 (F2) — in-tree symlink-to-dir glob arg double-keys the baseline ─────

/// F2 — a glob of an in-tree symlink-to-dir must key its contained file by the
/// **canonical** real path, so a later full walk (grep/diagnostics) does not
/// phantom-reap it as `Deleted`.
///
/// `collect_scoped_observations` walks a symlink-to-dir glob arg (`linkdir/`) at
/// its literal path and (pre-fix) baselines the contained file under
/// `linkdir/x.<EXT>`. But grep (`WalkBuilder::new(root)`) and diagnostics
/// (`stat_walk`) walk the canonical root with `follow_links` **off**, so they
/// never descend the in-tree link — they observe only the real path
/// `realdir/x.<EXT>`. The same physical file is thus baselined under two keys:
/// the glob's `linkdir/x.<EXT>` and the others' `realdir/x.<EXT>`. glob never
/// reaps, but the next pathless full grep observes only `realdir/x.<EXT>` and
/// reaps the orphan `linkdir/x.<EXT>` as a phantom `Deleted(3)` — telling every
/// covering server a live file is gone.
///
/// RED today: the full grep routes `Deleted(3)` for `linkdir/x.<EXT>`. The
/// decided fix canonicalizes glob's observed entries to the real path, so both
/// surfaces key `realdir/x.<EXT>` and no phantom `Deleted` is routed.
#[test]
#[ignore = "RED: WS31-review-C C1; un-ignore in fix"]
fn ws31_review_c1_symlink_dir_glob_single_canonical_key() -> Result<()> {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir()?;
    // Canonicalize the tempdir base so the ONLY symlink in play is `linkdir`
    // (some platforms route `/tmp` through a symlink; canonicalizing keeps the
    // root-relative keys robust).
    let base = dir.path().canonicalize()?;
    let log_path = base.join("notifications.jsonl");

    let realdir = base.join("realdir");
    std::fs::create_dir(&realdir)?;
    let real_file = realdir.join(format!("x.{MOCK_LANG}"));
    std::fs::write(&real_file, "needle\n")?;

    let linkdir = base.join("linkdir");
    symlink(&realdir, &linkdir)?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 7 (ALL) ⇒ registers Delete, so a spurious `Deleted` IS routed/recorded.
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG} \
             --watcher-kind 7 --notification-log {log_arg}"
        ),
    );
    let root = base.to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Seed the per-root baseline via a glob of the symlinked dir arg. Pre-fix
    // this keys the contained file under the literal `linkdir/x.<EXT>`.
    let linkdir_arg = linkdir.to_str().context("linkdir path")?;
    let _ = bridge.call_tool_text("glob", &json!({ "paths": [linkdir_arg] }))?;
    std::thread::sleep(Duration::from_millis(150));

    // Pathless full grep: the harness injects cwd=root, so ripgrep walks the
    // canonical root WITHOUT following `linkdir`, observing only
    // `realdir/x.<EXT>`. The reap sweep then fires over the baseline.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(300));

    let link_uri = format!("file://{}/linkdir/x.{MOCK_LANG}", base.display());
    let real_uri = format!("file://{}/realdir/x.{MOCK_LANG}", base.display());
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let changes = watched_file_changes(&log);

    // Key assertion (RED today): the orphan `linkdir/x.<EXT>` baseline key must
    // NOT be reaped — it is the same physical file as `realdir/x.<EXT>`, which
    // is present on disk. Pre-fix glob keys it literally and the full grep reaps
    // it as a phantom `Deleted(3)`.
    assert!(
        !changes.iter().any(|(u, t)| *u == link_uri && *t == 3),
        "an in-tree symlink-to-dir glob arg must not produce a phantom Deleted \
         for linkdir/x.<EXT> — it is the same file as realdir/x.<EXT>, present on \
         disk. changes={changes:?}, log:\n{log}"
    );
    // Companion guard: the real file IS tracked under its canonical key (so the
    // fix can't trivially pass by making glob stop nudging entirely). It enters
    // the baseline as Created(1) on the second (full-grep) walk.
    assert!(
        changes
            .iter()
            .any(|(u, t)| *u == real_uri && (*t == 1 || *t == 2)),
        "the contained file must be tracked under its canonical realdir key \
         (Created(1) or Changed(2)). changes={changes:?}, log:\n{log}"
    );
    Ok(())
}
