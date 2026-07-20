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
//! Two regression guards pinning the fixes for bugs C1 and H1 (both in
//! `src/bridge/grep_server.rs`'s ticket-04 reaping path). The bugs are fixed
//! (commit `7b9b847`) and both guards run in the normal suite (no `#[ignore]`d
//! `ws31_review` tests remain); each FAILS if its fix regresses.
//!
//! - **C1** (`ws31_review_r1_scoped_grep_no_spurious_delete`): a path-scoped
//!   enriched grep took `WalkBreadth::Full` whenever the root had covering
//!   watchers, so its partial observation set (only the scoped subtree) was fed
//!   to `diff_update_and_reap`, which reaped every baselined file outside the
//!   scope as `Deleted(3)`. The guard pins that out-of-scope files survive.
//! - **H1** (`ws31_review_r1_incomplete_observation_not_reaped`): a present file
//!   passing `is_file` via cached `d_type` whose *fresh* `metadata()` stat then
//!   races (here, EACCES from a parent with no execute bit) was omitted from the
//!   observation set with no retry / no prior-mtime fallback, so a full walk
//!   false-reaped it as `Deleted(3)`. The guard pins that it is no longer reaped.

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{
    BridgeProcess, grep_until_enriched, ipc_request, mockls_lsp_arg, poll_log_until,
    read_merged_log, rewrite_advancing_mtime, wait_for_change, wait_for_change_in_root,
};

// `MOCK_LANG`'s server key is the blessed `mockls-event` persona
// (diagnostics-debt 04c): manifest membership is what makes a mock a diagnostics
// source, so the two diagnostics-surface guards (C4, D) that drive
// `call_diagnostics` get a real source. The empty behavior bundle is plain
// default-push mockls — the grep/watcher guards that share `MOCK_LANG` see no
// wire change. `MOCK_LANG_S` stays a non-persona key: it serves only the L5
// grep/watcher test (enrichment-only, no diagnostics assertion), and a diagnostics
// source is not needed — nor could it collide, since it must be a DISTINCT key
// from `mockls-event` (both servers run at once on nested roots).
const MOCK_LANG: &str = "mockls-event";
/// Second mock language for the L5 nested-root test (a distinct server).
const MOCK_LANG_S: &str = "z9Qw7";

/// C1 — a path-scoped enriched grep must not reap files outside its scope.
///
/// The baseline is seeded by a pathless full grep (the harness injects cwd=root,
/// so the walk covers both `a/` and `b/`, baselining `a/match` and `b/keep`).
/// A second grep scoped to `a/` walks only `a/`, so its observation set omits
/// `b/keep`. Pre-fix, because the root has covering watchers, that scoped walk
/// still took `WalkBreadth::Full` and called `diff_update_and_reap` with
/// `reap=true` — which reaped `b/keep` (present on disk!) as `Deleted(3)`. The
/// fix (commit `7b9b847`) gates the reap on whether the walk spanned the root;
/// this guard pins it green.
#[test]
fn ws31_review_r1_scoped_grep_no_spurious_delete() -> Result<()> {
    // Canonical tempdir: the daemon's coherence-walk URIs are canonical, so the
    // `dir.path()`-derived expected URIs must be too (macOS symlinked-tempdir
    // class; a symlinked `TMPDIR` on Linux reproduces it).
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");

    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir(&a)?;
    std::fs::create_dir(&b)?;
    let a_match = a.join(format!("match.{MOCK_LANG}"));
    let b_keep = b.join(format!("keep.{MOCK_LANG}"));
    // Probe bait (bug 133 lean 2): the eager probe holds the sorted-first
    // matching file OPEN, and an open document routes external changes as
    // the didChange relay, never watched-files — so keep the files under
    // test out of the probe's pick.
    std::fs::write(
        dir.path().join(format!("_probe_bait.{MOCK_LANG}")),
        "bait\n",
    )?;
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
    // the baseline. The baseline is recorded synchronously inside the grep call's
    // `nudge_changed_set`, so no settle wait is needed after it returns.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // Advance a/match's mtime so the SCOPED walk itself observes it as changed and
    // routes a fresh `Changed(2)`. Without this the companion guard below would be
    // satisfied by walk #1's cold-start emission alone (a/match is unchanged on
    // the second walk ⇒ empty change-set ⇒ early return), so the companion would
    // pass even if the scoped walk routed nothing — making it non-load-bearing.
    // `rewrite_advancing_mtime` gates on the observed mtime advance (the change
    // signal) instead of a fixed sleep to span mtime granularity.
    rewrite_advancing_mtime(&a_match, "needle\nneedle\n")?;

    // Scoped grep: ripgrep walks only a/. Its observation set omits b/keep.
    let a_str = a.to_str().context("a path")?;
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle", "paths": [a_str] }))?;

    let a_match_uri = format!("file://{}/a/match.{MOCK_LANG}", dir.path().display());
    let b_keep_uri = format!("file://{}/b/keep.{MOCK_LANG}", dir.path().display());

    // Anchor on the positive completion signal: poll the live log until the
    // scoped walk's Changed(2) for the in-scope a/match appears (proving the
    // scoped walk ran and its notifications flushed to mockls), then assert the
    // out-of-scope b/keep is NOT reaped in that SAME snapshot. The bridge stays
    // alive while polling — mockls writes the log on receipt — so there is no
    // shutdown/flush race. No fixed sleep guesses "long enough".
    let changes = wait_for_change(&log_path, &a_match_uri, 2);
    let log = read_merged_log(&log_path);

    // Companion guard: the SCOPED walk itself must route a/match as Changed(2)
    // (its mtime was advanced above), so the fix can't trivially pass by making
    // the scoped grep stop nudging entirely — the change-set is non-empty and the
    // scoped walk is exercised, not just walk #1's cold-start emission. This is
    // also the positive anchor the negative below relies on.
    assert!(
        changes.iter().any(|(u, t)| *u == a_match_uri && *t == 2),
        "the in-scope file a/match must still be routed as Changed(2) by the \
         scoped walk. changes={changes:?}, log:\n{log}"
    );
    // Key assertion (green guard): the scoped grep must NOT reap b/keep — it is
    // present on disk and merely outside the scoped walk's breadth.
    assert!(
        !changes.iter().any(|(u, t)| *u == b_keep_uri && *t == 3),
        "scoped grep must not reap files outside its scope; b/keep is present. \
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
    // Canonicalized so the `dir.path()`-derived expected URIs match the daemon's
    // canonical coherence-walk URIs (macOS symlinked-tempdir class). `/tmp` is
    // already canonical on Linux, so the tmpfs/`d_type` precondition is preserved.
    let dir = common::canonical_tempdir()?;
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
    // sub/locked and keep baselined. The baseline is recorded synchronously
    // inside the grep call, so no settle wait is needed after it returns.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // Create a non-edited `witness` AFTER the baseline seed: on the SECOND walk
    // it is absent-on-a-populated-baseline ⇒ routed as Created(1). A Created(1)
    // can only come from the second full walk, so it is the positive completion
    // signal that the reap sweep ran over the root — the anchor the negative
    // (absence) assertion below polls on, since absence itself cannot be polled.
    let witness = dir.path().join(format!("witness.{MOCK_LANG}"));
    std::fs::write(&witness, "needle\n")?;

    // Strip execute (search) from sub: readdir still works (read granted) so
    // sub/locked is enumerated with cached d_type and passes `is_file`, but a
    // fresh metadata() stat on it needs execute on sub → EACCES → omitted from
    // observations → false-reaped on the full walk. sub/locked is still on disk.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o400))?;

    // Pathless full grep again — covers the whole tree; reap sweep fires.
    let grep_result = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }));

    // RESTORE execute immediately, in the test BODY: the tempdir's Drop must
    // recurse into sub to clean up, which needs execute.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700))?;
    grep_result?;

    let locked_uri = format!("file://{}/sub/locked.{MOCK_LANG}", dir.path().display());
    let witness_uri = format!("file://{}/witness.{MOCK_LANG}", dir.path().display());

    // Anchor on the positive completion signal: poll the live log until the
    // second walk routes `witness` as Created(1) (proving the full stat-walk +
    // reap sweep ran and flushed), then assert the present-but-unstattable
    // sub/locked is NOT reaped in that SAME snapshot. The bridge stays alive
    // while polling — mockls writes the log on receipt — so no shutdown race and
    // no fixed-time "long enough" guess.
    let changes = wait_for_change(&log_path, &witness_uri, 1);
    let log = read_merged_log(&log_path);

    // Companion guard (non-vacuous): the `witness`, created after the seed, must
    // be routed Created(1) by the second full walk, pinning that the walk + reap
    // sweep ran so the key assertion can't pass by walking/routing nothing.
    assert!(
        changes.iter().any(|(u, t)| *u == witness_uri && *t == 1),
        "the non-edited `witness` (created after the seed) must be routed as \
         Created(1) by the second full walk, proving the reap sweep ran. \
         changes={changes:?}, log:\n{log}"
    );
    // Key assertion (green guard): a present file whose fresh metadata() raced
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
    // The assertions read only this grep's OWN result text (returned
    // synchronously), not the notification log, so no settle wait is needed —
    // there is no async signal to wait for. The bridge drops at end of scope.
    let out = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

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
/// re-grep was cold by counting `textDocument/documentSymbol` on the re-spawned
/// server: a no-op evict would leave the daemon-lived `SymbolIndex` warm, the
/// re-grep would short-circuit on the cache, and the re-spawned server would see
/// ZERO `documentSymbol`.
///
/// Witness via PER-PID request logs (`--log-pid-suffix`), not one shared file.
/// Under load the per-root server can transiently double-spawn across the rm→add
/// cycle (a fast-`mockls` timing artifact: a real, slow-init LSP serializes on the
/// spawn lock and never double-spawns — verified over 90 cycles with rust-analyzer
/// in `ra_double_spawn_probe.rs`). With ONE shared `File::create`-truncated log
/// that second instance wipes the first's logged cold request → spurious
/// `count=0`. Each instance instead writes its own `requests.jsonl.<pid>`;
/// warm-phase pid logs are deleted after `roots-rm`, so any request surviving in a
/// post-re-add log is necessarily the COLD re-query, and the merge across all
/// post-re-add logs is immune to cross-instance truncation.
///
/// GREEN today: `evict_root` is correct ⇒ the re-grep is cold ⇒ documentSymbol
/// is re-issued. The mutation check (Phase-A writer stubbed `evict_root` to a
/// no-op) confirms this FAILS when eviction is broken.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "linear: setup + rm/add cycle + asserts"
)]
fn ws31_review_r4_eviction_witnessed_via_request_count() -> Result<()> {
    // Per-pid request logs written next to the base as `requests.jsonl.<pid>`.
    fn list_pid_logs(base: &std::path::Path) -> Vec<std::path::PathBuf> {
        let Some(dir) = base.parent() else {
            return Vec::new();
        };
        let Some(name) = base.file_name().and_then(|n| n.to_str()) else {
            return Vec::new();
        };
        let prefix = format!("{name}.");
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name()
                    .to_str()
                    .is_some_and(|f| f.starts_with(&prefix))
                {
                    out.push(e.path());
                }
            }
        }
        out
    }

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

    // Per-pid request logs: `requests.jsonl.<pid>`, one writer each (no tearing).
    let req_log_base = work_path.join("requests.jsonl");
    let req_log_arg = req_log_base.to_str().context("request log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!("--scan-roots --request-log {req_log_arg}"),
    );

    let mut bridge = BridgeProcess::spawn(&[&lsp], base_str)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // Add and warm the work root. Retry the warming grep until the `calls:`
    // enrichment signal appears instead of sleeping to guess the per-root server
    // + `--scan-roots` index is ready — the enrichment-present output IS the
    // readiness signal, so the warm grep can't race server readiness under load.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    bridge.wait_for_root(work_str, Duration::from_secs(5))?;

    let warm = grep_until_enriched(
        &bridge,
        &json!({ "pattern": "callee_x", "directory": work_str }),
    )?;
    // Precondition — the setup is genuinely enriched (the callee's in-body usage
    // carries the `#caller_x` containment anchor).
    assert!(
        warm.contains("#caller_x"),
        "warming grep must be enriched (callee usage carries `#caller_x`), got:\n{warm}"
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

    // Delete every warm-phase pid log: any request in a post-re-add log is COLD.
    for p in list_pid_logs(&req_log_base) {
        let _ = std::fs::remove_file(&p);
    }

    // Re-add the root. The per-root server re-spawns; its fresh
    // `requests.jsonl.<pid>` captures the re-grep's requests.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    bridge.wait_for_root(work_str, Duration::from_secs(5))?;

    // Re-grep. With a correct evict, the daemon-lived SymbolIndex was emptied for
    // this root, so this is a genuine cold touch: the daemon re-queries the
    // outline. Retry until the `calls:` enrichment signal appears rather than
    // sleeping to guess the re-spawned server is ready — that guess (300 ms) was
    // the flake source under CPU contention, racing a cold grep against the
    // re-spawn + `--scan-roots` index so the request counts came up empty. Each
    // retry re-issues documentSymbol/outgoingCalls into the (per-pid) request
    // log, so the `>= 1` counts below only strengthen.
    let cold = grep_until_enriched(
        &bridge,
        &json!({ "pattern": "callee_x", "directory": work_str }),
    )?;
    // Anti-vacuous: the re-grep still surfaces the symbol AND re-resolves enrichment.
    assert!(
        cold.contains("callee_x"),
        "re-added root must serve the raw match; got:\n{cold}"
    );
    assert!(
        cold.contains("#caller_x"),
        "cold first touch after eviction must re-resolve enrichment; got:\n{cold}"
    );

    // Drop the bridge first to shut the server(s) down — the on-exit flush is the
    // durability guarantee the request-log read relies on. The poll below removes
    // the fixed-time GUESS for when that flush becomes visible: under heavy load
    // the re-spawned server's requests may not be flushed by a fixed delay, so we
    // re-merge all post-re-add per-pid logs until both expected counts hold (or
    // the GENEROUS backstop passes). Merging is immune to the transient
    // double-spawn that a single shared `File::create` log would let truncate.
    drop(bridge);
    let merged = || -> (usize, String) {
        let mut buf = String::new();
        for p in list_pid_logs(&req_log_base) {
            if let Ok(t) = std::fs::read_to_string(&p) {
                buf.push_str(&t);
            }
        }
        let doc = request_method_count(&buf, "textDocument/documentSymbol");
        (doc, buf)
    };
    let req_deadline = std::time::Instant::now() + common::POLL_BACKSTOP;
    let (mut doc_symbol_count, mut req_log) = merged();
    while doc_symbol_count < 1 && std::time::Instant::now() < req_deadline {
        std::thread::sleep(common::POLL_SPACING);
        let m = merged();
        doc_symbol_count = m.0;
        req_log = m.1;
    }

    // The re-grep was cold ⇒ the daemon re-issued documentSymbol against the
    // re-spawned server. A no-op evict would leave the cache warm ⇒ count 0.
    // A deadline reached with the condition still unmet falls through to FAIL.
    // The `#scope` anchor comes from `documentSymbol` alone — the per-hit nav
    // suite (callHierarchy/outgoingCalls) no longer fires — so documentSymbol is
    // the sole cold-touch witness.
    assert!(
        doc_symbol_count >= 1,
        "re-add must be a genuine cold touch — the daemon must re-query the \
         outline (textDocument/documentSymbol) at least once. count={doc_symbol_count}, \
         merged request log:\n{req_log}"
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
/// Contention-safe absence anchor: "walk #2 emitted nothing" is an absence,
/// which cannot be polled directly. So a THIRD walk over a genuinely-new file
/// (`tail`) routes a `Created(1)` whose appearance is the positive completion
/// signal. The single mockls input pipe is FIFO and mockls is single-threaded,
/// so once `tail`'s Created(1) is in the log, any change walk #2 had (wrongly)
/// emitted — sent earlier on the same pipe — is already written too. The final
/// log must be EXACTLY `after_first` followed by `tail`'s single Created(1): no
/// re-announce from walk #2 in between. No fixed sleep guesses "long enough".
///
/// GREEN today.
#[test]
fn ws31_review_r4_second_walk_emits_empty_changeset() -> Result<()> {
    // Canonical tempdir so the `dir.path()`-derived expected URIs match the
    // daemon's canonical coherence-walk URIs (macOS symlinked-tempdir class).
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    // Probe bait (bug 133 lean 2): keeps `a` (the sorted-first fixture file)
    // out of the eager probe's held-open pick, so both a and b route as
    // watched-files rather than the open-document didChange relay.
    std::fs::write(
        dir.path().join(format!("_probe_bait.{MOCK_LANG}")),
        "bait\n",
    )?;
    std::fs::write(dir.path().join(format!("a.{MOCK_LANG}")), "needle\n")?;
    std::fs::write(dir.path().join(format!("b.{MOCK_LANG}")), "other\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 7 (ALL) registers Create, so walk #3's `tail` Created(1) anchor is
    // routed/recorded (the coherence walk observes every visited file, so b is in
    // the candidate set regardless of its `other` content not matching `needle`).
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

    let a_uri = format!("file://{}/a.{MOCK_LANG}", dir.path().display());
    let b_uri = format!("file://{}/b.{MOCK_LANG}", dir.path().display());

    // Walk #1 — the cold-start full candidate set. Poll the live log until BOTH
    // a and b are announced (the positive signal that walk #1 flushed), then
    // snapshot that as `after_first`.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    let after_first = poll_log_until(&log_path, |changes| {
        changes.iter().any(|(u, _)| *u == a_uri) && changes.iter().any(|(u, _)| *u == b_uri)
    });
    // Anti-vacuous: the first walk announced the cold candidate set.
    assert!(
        !after_first.is_empty(),
        "first walk must announce the cold candidate set. after_first={after_first:?}"
    );

    // Walk #2 — NO FS change. Must emit zero new changes.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // Walk #3 over a genuinely-new file: routes `tail` as Created(1), the FIFO
    // completion anchor that proves walk #2's (absent) emissions are already
    // written to the log.
    let tail = dir.path().join(format!("tail.{MOCK_LANG}"));
    std::fs::write(&tail, "needle\n")?;
    let tail_uri = format!("file://{}/tail.{MOCK_LANG}", dir.path().display());
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;
    let total = wait_for_change(&log_path, &tail_uri, 1);

    // Positively encodes bug-38 no-repeat: walks #2 added zero changes, so the
    // full set is EXACTLY `after_first` followed by walk #3's single Created(1)
    // for `tail`. Order-stable single-server log. Any re-announce by walk #2
    // would appear between `after_first` and `tail` and break this equality.
    let mut expected = after_first;
    expected.push((tail_uri, 1));
    assert_eq!(
        total, expected,
        "the no-change walk #2 must emit zero new didChangeWatchedFiles (only \
         walk #3's tail Created(1) is appended after the first walk's set). \
         total={total:?}, expected={expected:?}"
    );

    Ok(())
}

// ── R4 (L5) — covering_watchers subdir/parent scope prefix matching ─────────

/// L5 — `covering_watchers` includes a subdir-scoped server for a parent walk and
/// excludes a parent-scoped server for a child-grouped file
/// (`scope.root_path().starts_with(root)`).
///
/// Two canonicalized tracked roots `parent` and `parent/sub`, each language
/// registering its own glob. Under the per-root server architecture (one instance
/// per tracked root), each language has TWO instances — one at `parent`, one at
/// `parent/sub` — all handed the SAME `--notification-log` path. Two instances
/// appending to one file tear each other's JSONL lines, which no signal poll can
/// parse, so `--log-pid-suffix` gives each instance its OWN `<base>.<pid>` file;
/// the test merges all of a language's files (`common::*_merged`) to ask "did ANY
/// instance of this server record change X".
///
/// - **Positive:** a grep over `parent` observes `parent/top.<LANG_S>` (root
///   `parent`). An S instance scoped to `parent/sub` is INCLUDED in the
///   `parent`-root nudge because `parent/sub` `starts_with` `parent`, so some S
///   file records the `top.<LANG_S>` change.
/// - **Negative:** `parent/sub/inner.<LANG_P>` resolves (longest-prefix) to root
///   `parent/sub`. `covering_watchers(parent/sub)` EXCLUDES the P instance scoped
///   to `parent` (`parent` does NOT `starts_with` `parent/sub`), and S's glob
///   excludes `.<LANG_P>`, so NO P file ever records `inner.<LANG_P>`. The
///   absence is anchored on a positive P signal (a fresh `anchor.<LANG_P>` under
///   `parent`, routed Created(1) to a P instance) so the negative is asserted
///   only after a P walk provably ran and flushed — not after a fixed sleep.
///
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

    // Server P (lang = MOCK_LANG) and server S (lang = MOCK_LANG_S). Each spawns
    // one instance per tracked root, so each log BASE is split into `<base>.<pid>`
    // per instance via `--log-pid-suffix` (read merged below).
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

    // Positive fixture: top_s (S-glob, directly under `parent`) is grouped under
    // root `parent`; an `inner.<LANG_S>` under `parent/sub` makes the daemon spawn
    // an S instance ROOTED at `parent/sub` (per-root instances are spawned where
    // matching files are found). That parent/sub-scoped S instance must then be
    // INCLUDED in the `parent`-root nudge (parent/sub starts_with parent) and so
    // receive top_s — that is the inclusion the positive proves. top_p (P-glob)
    // makes the parent grep walk `parent`.
    let top_s = parent_path.join(format!("top.{MOCK_LANG_S}"));
    let top_p = parent_path.join(format!("top.{MOCK_LANG}"));
    let inner_s = sub_path.join(format!("inner.{MOCK_LANG_S}"));
    std::fs::write(&top_s, "needle\n")?;
    std::fs::write(&top_p, "needle\n")?;
    std::fs::write(&inner_s, "needle\n")?;
    // Negative fixture: a P-glob file under `parent/sub`, grouped under root
    // `parent/sub` (longest-prefix), whose covering set excludes the `parent` P.
    let inner_p = sub_path.join(format!("inner.{MOCK_LANG}"));
    std::fs::write(&inner_p, "needle\n")?;

    // Both roots tracked: `parent` and `parent/sub`. Declare them via MCP
    // `roots/list` so they enter the daemon's RootTracker (a plain `initialize`
    // would leave the tracker empty, so `roots-ls` would never report them).
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp_p, &lsp_s], &[parent_str, sub_str])?;
    bridge.initialize_with_roots(&[parent_str, sub_str])?;
    bridge.wait_for_root(parent_str, Duration::from_secs(5))?;
    bridge.wait_for_root(sub_str, Duration::from_secs(5))?;

    // POSITIVE walk: grep over `parent`. The walk covers root `parent`; the S
    // instance scoped to `parent/sub` is a covering watcher because
    // `parent/sub`.starts_with(`parent`). `inner_p` (`.MOCK_LANG` under
    // `parent/sub`) resolves to root `parent/sub`, whose covering set EXCLUDES the
    // `parent`-scoped P, so no P instance ever receives it.
    let _ = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "directory": parent_str }),
    )?;

    // NEGATIVE walk: a SEPARATE grep tightly scoped to `parent/sub`. The walk root
    // is `parent/sub`; the `parent`-scoped P is EXCLUDED because
    // `parent`.starts_with(`parent/sub`) is false.
    let _ = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "directory": sub_str }),
    )?;

    let top_s_uri = format!("file://{}/top.{MOCK_LANG_S}", parent_path.display());
    let inner_p_uri = format!("file://{}/sub/inner.{MOCK_LANG}", parent_path.display());

    // Positive: the S instance scoped to `parent/sub` must be INCLUDED in the
    // `parent`-root nudge (parent/sub starts_with parent) and record top.<LANG_S>
    // (top_s is directly under `parent`, root `parent`). Read the SPECIFIC
    // parent/sub-scoped S instance's log — not the merged view — so the inclusion
    // of THAT instance (not just any S) is what's proven. Poll until top_s
    // appears: the positive completion signal, no fixed settle wait.
    let s_changes = wait_for_change_in_root(&log_s, sub_str, &top_s_uri, 2);
    let log_s_text = common::read_instance_log_for_root(&log_s, sub_str);
    assert!(
        s_changes.iter().any(|(u, _)| *u == top_s_uri),
        "the S instance scoped to parent/sub must be INCLUDED in the parent-root \
         nudge (parent/sub starts_with parent) and receive top.{MOCK_LANG_S}. \
         s_changes={s_changes:?}, log:\n{log_s_text}"
    );

    // Negative anchor: the `parent`-scoped P instance never receiving inner_p is
    // an absence (un-pollable). inner_p resolves (longest-prefix) to root
    // `parent/sub`, whose covering set EXCLUDES the `parent`-scoped P. (The
    // `parent/sub`-scoped P instance DOES receive it — which is correct, and why
    // the per-instance log read below targets the `parent` instance only; reading
    // a merged P log would wrongly see the parent/sub instance's copy.) Both walks
    // have returned, so any hypothetical buggy inner_p→P@parent nudge was already
    // sent. Route a FRESH legitimate change to P@parent and wait for it: a new
    // P-glob file directly under `parent` resolves to root `parent` (covering set
    // INCLUDES P@parent) and is routed Created(1). Polling P@parent's own log
    // until that anchor appears proves P@parent's walk ran and flushed, so the
    // absence is asserted over a snapshot that has seen everything up to it.
    let p_anchor = parent_path.join(format!("anchor.{MOCK_LANG}"));
    std::fs::write(&p_anchor, "needle\n")?;
    let p_anchor_uri = format!("file://{}/anchor.{MOCK_LANG}", parent_path.display());
    let _ = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "directory": parent_str }),
    )?;
    let p_changes = wait_for_change_in_root(&log_p, parent_str, &p_anchor_uri, 1);
    let log_p_text = common::read_instance_log_for_root(&log_p, parent_str);

    // Companion (proves the parent-scoped P instance's walk/pipe is drained past
    // the anchor point).
    assert!(
        p_changes.iter().any(|(u, t)| *u == p_anchor_uri && *t == 1),
        "the parent-scoped P instance must receive the fresh anchor.{MOCK_LANG} \
         (under root parent) as Created(1), proving its nudge pipe is drained. \
         p_changes={p_changes:?}, log:\n{log_p_text}"
    );
    // Negative: the parent-scoped P instance must NOT have received the
    // child-grouped file (parent does NOT start_with parent/sub).
    assert!(
        !p_changes.iter().any(|(u, _)| *u == inner_p_uri),
        "the parent-scoped P instance must be EXCLUDED from the parent/sub root \
         (parent does NOT start_with parent/sub) and must not receive \
         sub/inner.{MOCK_LANG}. p_changes={p_changes:?}, log:\n{log_p_text}"
    );

    Ok(())
}

// ── C4 (L6) — an edited-then-deleted file's Deleted is routed, not suppressed ──

/// L6 — a file that is in the diagnostics edited-set AND then deleted from disk
/// must still be reaped as `Deleted` (wire `FileChangeType` 3) and that Delete
/// ROUTED, not suppressed by the diagnostics `exclude` set — even when the same
/// batch carries a live edited sibling.
///
/// The review's L6 premise was that an edited path placed in the diagnostics
/// `exclude` set would suppress a reaped `Deleted` for that path. Verified
/// WRONG: `process_files_batched` builds `exclude` from `canonical_paths`, and a
/// path only enters `canonical_paths` after passing `validate_read`
/// (`diagnostics_server.rs:189`/`:201`). `validate_read` calls
/// `path.canonicalize()` (`path_security.rs:79`), which FAILS for a file no
/// longer on disk — so an edited-then-deleted file never enters
/// `canonical_paths`, never enters `exclude`, and its reaped `Deleted` is routed
/// by `nudge_changed_set` (the `exclude.contains(&change.rel)` guard is false).
///
/// This is a GREEN guard (NOT `#[ignore]`d) that is **load-bearing for the L6
/// fix**: `gone` is driven through the edited-set INTERSECTION the fix is about.
/// It is accumulated into the edited-set via `call_diagnostics_multi` (the
/// `pre-tool/editing-state` `Edit` path) AND deleted from disk, so on the
/// diagnostics stop it flows through `validate_read` exactly where the L6
/// suppression would land. If a future change put raw edited paths into
/// `exclude` BEFORE the existence/canonicalize check (so an edited-then-deleted
/// path landed in `exclude`), the `exclude.contains` guard would fire and this
/// guard would FAIL — which is exactly the L6 suppression the review feared (and
/// which does NOT occur today). The prior version deleted `gone` externally and
/// never passed it to diagnostics, so `gone` never entered the edited-set and
/// reverting the L6 fix would not have failed it (it only re-proved the
/// externally-deleted reap already covered by
/// `changed_set.rs::diagnostics_full_walk_reaps_deletion`).
///
/// The live sibling rides the same batch via the edited-set and keeps the root
/// in the diagnostics batch's `roots` set (built from `canonical_paths`) so the
/// full stat-walk runs over the root and the reap sweep fires — a batch with no
/// canonicalizable file would walk nothing. kind 7 (ALL) registers Delete, so a
/// routed `Deleted` IS recorded in the log.
#[test]
fn ws31_review_c4_edited_then_deleted_drives_exclude() -> Result<()> {
    // Canonical tempdir so the `dir.path()`-derived expected URIs match the
    // daemon's canonical coherence-walk URIs (macOS symlinked-tempdir class).
    let dir = common::canonical_tempdir()?;
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
    // enter the baseline. The baseline is recorded synchronously inside the
    // diagnostics call, so no settle wait is needed after it returns.
    let live_str = live.to_str().context("live path")?;
    let gone_str = gone.to_str().context("gone path")?;
    let _ = bridge.call_diagnostics(live_str)?;

    // Delete `gone` from disk while it is about to ride the edited-set: the
    // second diagnostics batch carries BOTH `live` (present) and `gone` (deleted)
    // as edited files. `gone` is the edited-then-deleted INTERSECTION — it drives
    // through `validate_read`, whose canonicalize FAILS, so it never lands in the
    // `exclude` set. `live` keeps the root in the batch's `roots` set so the full
    // stat-walk runs and reaps `gone`.
    std::fs::remove_file(&gone)?;

    // Second diagnostics run over the edited-set {live, gone}: drains the
    // edited-set ⇒ a batch over the root ⇒ the full stat-walk reaps `gone`
    // (baselined but no longer observed) ⇒ routed because `gone` is NOT in
    // `exclude`.
    let _ = bridge.call_diagnostics_multi(&[live_str, gone_str])?;

    let gone_uri = format!("file://{}/gone.{MOCK_LANG}", dir.path().display());

    // Poll the live log until the reaped `gone` Deleted(3) appears (the positive
    // completion signal that the reap was routed and flushed to mockls), then
    // assert over that snapshot. No fixed sleep, no shutdown/flush race.
    let changes = wait_for_change(&log_path, &gone_uri, 3);
    let log = read_merged_log(&log_path);

    // GREEN today, load-bearing for L6: the edited-then-deleted file's `Deleted`
    // must be ROUTED. `gone` is in the edited-set but NOT in `exclude` —
    // `validate_read` drops it (canonicalize fails for a missing path) before it
    // could enter `canonical_paths`/`exclude` — so the reaped `Deleted` is
    // delivered. Putting raw edited paths into `exclude` before the existence
    // check (the L6 regression) would suppress this and FAIL the assert.
    assert!(
        changes.iter().any(|(u, t)| *u == gone_uri && *t == 3),
        "an edited-then-deleted file's Deleted must be routed (it is in the \
         edited-set but NOT in exclude — validate_read drops it); \
         changes={changes:?}, log:\n{log}"
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
/// The fix canonicalizes glob's observed entries to the real path, so both
/// surfaces key `realdir/x.<EXT>` and no phantom `Deleted` is routed.
#[test]
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
    // Probe bait (bug 133 lean 2): keeps `realdir/x` out of the eager
    // probe's held-open pick, so its baseline events route as watched-files.
    std::fs::write(base.join(format!("_probe_bait.{MOCK_LANG}")), "bait\n")?;
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
    // this keys the contained file under the literal `linkdir/x.<EXT>`. The
    // baseline is recorded synchronously inside the glob call.
    let linkdir_arg = linkdir.to_str().context("linkdir path")?;
    let _ = bridge.call_tool_text("glob", &json!({ "paths": [linkdir_arg] }))?;

    // Pathless full grep: the harness injects cwd=root, so ripgrep walks the
    // canonical root WITHOUT following `linkdir`, observing only
    // `realdir/x.<EXT>`. The reap sweep then fires over the baseline.
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    let link_uri = format!("file://{}/linkdir/x.{MOCK_LANG}", base.display());
    let real_uri = format!("file://{}/realdir/x.{MOCK_LANG}", base.display());

    // Anchor on the positive completion signal: poll the live log until the full
    // grep walk tracks the real file under its canonical realdir key (Created(1)
    // or Changed(2)) — proving the walk ran and flushed — then assert the orphan
    // linkdir key is NOT reaped in that SAME snapshot. No fixed sleep.
    let changes = poll_log_until(&log_path, |c| {
        c.iter()
            .any(|(u, t)| *u == real_uri && (*t == 1 || *t == 2))
    });
    let log = read_merged_log(&log_path);

    // Companion guard: the real file IS tracked under its canonical key (so the
    // fix can't trivially pass by making glob stop nudging entirely). It enters
    // the baseline as Created(1) on the second (full-grep) walk. This is also the
    // positive anchor the negative below relies on.
    assert!(
        changes
            .iter()
            .any(|(u, t)| *u == real_uri && (*t == 1 || *t == 2)),
        "the contained file must be tracked under its canonical realdir key \
         (Created(1) or Changed(2)). changes={changes:?}, log:\n{log}"
    );
    // Key assertion (regression guard): the orphan `linkdir/x.<EXT>` baseline
    // key must NOT be reaped — it is the same physical file as `realdir/x.<EXT>`,
    // which is present on disk. Pre-fix (C1/F2) glob keyed it literally and the
    // full grep reaped it as a phantom `Deleted(3)`.
    assert!(
        !changes.iter().any(|(u, t)| *u == link_uri && *t == 3),
        "an in-tree symlink-to-dir glob arg must not produce a phantom Deleted \
         for linkdir/x.<EXT> — it is the same file as realdir/x.<EXT>, present on \
         disk. changes={changes:?}, log:\n{log}"
    );
    Ok(())
}

// ── D4 (T4) — diagnostics stat-miss must not false-reap a present file ──────

/// T4 — the diagnostics full stat-walk must not reap a present file whose fresh
/// `metadata()` stat races (EACCES).
///
/// This is the **diagnostics-surface** counterpart of the grep H1 guard
/// (`ws31_review_r1_incomplete_observation_not_reaped`): it drives the same
/// EACCES seam through `catenary diagnostics` instead of `grep`, exercising
/// `diagnostics_server.rs::stat_walk` → `nudge_changed_set(..., reap=true)`. The
/// C1/F1 fix wired `stat_walk` to the shared `observe_mtime` helper
/// (sentinel-on-miss, never omit); the only prior guard tested the
/// `observe_mtime_with` helper in isolation, so reverting `stat_walk` to a bare
/// `if let Ok(md) = path.metadata()` (omit-on-miss) failed NO test. This guard
/// closes that gap: it FAILS (a phantom `Deleted(3)` for the present file) if
/// `stat_walk` regresses to omit-on-miss.
///
/// Mechanism mirrors H1 exactly. PRECONDITION: the tempdir lives on a filesystem
/// that populates `d_type` in readdir (tmpfs, where `tempfile::tempdir()` lands
/// by default). `stat_walk` uses the same `WalkBuilder` + cached-`d_type`
/// `is_file` decision as the grep walker, so `sub/locked` passes `is_file` from
/// `d_type` even when `sub` has no execute bit, but the separate fresh
/// `metadata()` (which needs execute on `sub`) fails EACCES — pre-fix dropping it
/// from the observation set → false-reaped on the full walk. On a `DT_UNKNOWN`
/// filesystem the failing stat would route through the `is_file` gate
/// (skip-as-non-file), masking the bug; the tmpfs default avoids that. The
/// `seam_is_ineffective()` probe skips under root / a permission-ignoring FS
/// (where the EACCES seam never fires → false GREEN).
///
/// The edited file that drives the batch (`anchor`) is deliberately NOT the
/// non-vacuous companion: diagnostics excludes edited paths from watched-file
/// routing (they ride document-sync), so an edited file never appears in
/// `didChangeWatchedFiles`. Instead a separate non-edited `witness` is created
/// AFTER the baseline seed, so it routes `Created(1)` on the SECOND walk only —
/// an unambiguous proof the second full stat-walk + reap sweep ran over the root.
#[test]
fn ws31_review_d_diagnostics_stat_miss_not_reaped() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Root / permission-ignoring-FS guard (shared with the grep H1 test): the
    // EACCES seam cannot be exercised there, so skip rather than false-GREEN.
    if seam_is_ineffective() {
        return Ok(());
    }

    // DEFAULT tempdir() → tmpfs, which populates d_type. See precondition.
    // Canonicalized so the `dir.path()`-derived expected URIs match the daemon's
    // canonical coherence-walk URIs (macOS symlinked-tempdir class); `/tmp` is
    // already canonical on Linux, so the tmpfs/`d_type` precondition holds.
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");

    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub)?;
    let locked = sub.join(format!("locked.{MOCK_LANG}"));
    // The edited file: it drives the diagnostics batch's `roots` set (built from
    // canonicalizable edited paths) so the full stat-walk + reap sweep fires.
    // Diagnostics excludes edited paths from watched-file routing, so `anchor`
    // never appears in the log — that is why a separate `witness` proves the walk.
    let anchor = dir.path().join(format!("anchor.{MOCK_LANG}"));
    std::fs::write(&locked, "needle\n")?;
    std::fs::write(&anchor, "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    // kind 7 (ALL) ⇒ registers Delete, so a spurious `Deleted` IS routed/recorded.
    // A kind without the Delete bit would mask the bug at routing.
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

    // Seed the per-root baseline with a first diagnostics run over `anchor`: its
    // full stat-walk observes the whole root, so BOTH `anchor` and `sub/locked`
    // enter the baseline. The baseline is recorded synchronously inside the call.
    let anchor_str = anchor.to_str().context("anchor path")?;
    let _ = bridge.call_diagnostics(anchor_str)?;

    // Create a non-edited `witness` AFTER the baseline seed: on the SECOND walk it
    // is absent-on-a-populated-baseline ⇒ routed as Created(1). A Created(1) can
    // only come from the second walk, so it unambiguously proves the second full
    // stat-walk (the reap sweep) ran over the root — the key assertion below
    // cannot pass by an early empty-change-set return.
    let witness = dir.path().join(format!("witness.{MOCK_LANG}"));
    std::fs::write(&witness, "needle\n")?;

    // Strip execute (search) from sub: readdir still works (read granted) so
    // sub/locked is enumerated with cached d_type and passes `is_file`, but a
    // fresh metadata() stat on it needs execute on sub → EACCES. Pre-fix it is
    // omitted from observations → false-reaped on the full walk. sub/locked is
    // still on disk.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o400))?;

    // Second diagnostics run over `anchor`: full stat-walk over the covered root
    // ⇒ the reap sweep fires. `witness` is observed (Created); `sub/locked` is
    // enumerated but its fresh stat races EACCES.
    let diag_result = bridge.call_diagnostics(anchor_str);

    // RESTORE execute immediately, in the test BODY: the tempdir's Drop must
    // recurse into sub to clean up, which needs execute.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700))?;
    diag_result?;

    let locked_uri = format!("file://{}/sub/locked.{MOCK_LANG}", dir.path().display());
    let witness_uri = format!("file://{}/witness.{MOCK_LANG}", dir.path().display());

    // Anchor on the positive completion signal: poll the live log until the
    // second walk routes `witness` Created(1) (proving the full stat-walk + reap
    // sweep ran and flushed), then assert the present-but-unstattable sub/locked
    // is NOT reaped in that SAME snapshot. No fixed sleep, no shutdown race.
    let changes = wait_for_change(&log_path, &witness_uri, 1);
    let log = read_merged_log(&log_path);

    // Companion guard (non-vacuous): the non-edited `witness`, created after the
    // baseline seed, is routed as Created(1) by the SECOND full stat-walk. This
    // pins that the walk + reap sweep ran over the root, so the key assertion
    // can't pass by walking/routing nothing — it is also the positive anchor the
    // negative below polls on (absence cannot be polled).
    assert!(
        changes.iter().any(|(u, t)| *u == witness_uri && *t == 1),
        "the non-edited `witness` (created after the seed) must be routed as \
         Created(1) by the second full stat-walk, proving the reap sweep ran. \
         changes={changes:?}, log:\n{log}"
    );
    // Key assertion (green guard): a present file whose fresh metadata() raced
    // (EACCES) must NOT be reaped by the diagnostics full stat-walk. Pre-fix
    // (`stat_walk` omitting on stat miss) routed a phantom `Deleted(3)`.
    assert!(
        !changes.iter().any(|(u, t)| *u == locked_uri && *t == 3),
        "the diagnostics full stat-walk must not reap a present file whose fresh \
         metadata() raced (EACCES); sub/locked is present on disk. \
         changes={changes:?}, log:\n{log}"
    );
    Ok(())
}
