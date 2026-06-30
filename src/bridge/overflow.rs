// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Runtime-dir overflow reports shared by the truncating surfaces.
//!
//! `catenary diagnostics` (cli-prerelease ticket 11) and the `catenary
//! grep`/`glob`/`sed` overflow [`valve`] (pipeable-output tickets 03 / 03a) bound
//! their stdout in memory and spill the *complete* output to a file under
//! `<runtime_dir>/catenary/` when that bound is hit, so the agent can read or
//! `catenary grep` the dropped tail. This module owns the one writer; the surfaces
//! differ in how they pointer the spill: diagnostics ends the truncated body with
//! an in-band `… full … at <path>` line, while the grep/glob/sed [`valve`] keeps
//! stdout byte-clean and returns the pointer as a separate stderr
//! [`receipt`](Valved::receipt).
//!
//! The two surfaces also differ in scope key and GC:
//!
//! - **scope key** — diagnostics names its file per live session
//!   (`diagnostics-<session_id>.txt`, overwritten each run); the valve mints a
//!   fresh per-invocation UUID (`<prefix><uuid>.txt`, never overwritten) because a
//!   query/preview is a stateless request (grep-class) with no session to key on.
//! - **GC** — diagnostics rides the session prune ([`sweep_diagnostics`] removes
//!   files whose id is no longer alive); the valve's spills have no session, so a
//!   daemon-startup [`sweep_query`] clears every leftover and an in-lifetime
//!   last-N cap ([`MAX_QUERY_OVERFLOW_FILES`]) bounds a long-running daemon
//!   between restarts.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

/// Filename prefix for per-session diagnostics overflow reports.
const DIAGNOSTICS_PREFIX: &str = "diagnostics-";
/// `.txt` suffix shared by every report kind.
const SUFFIX: &str = ".txt";

/// Filename prefix for `catenary grep` overflow spill files.
pub const GREP_PREFIX: &str = "grep-";
/// Filename prefix for `catenary glob` overflow spill files.
pub const GLOB_PREFIX: &str = "glob-";
/// Filename prefix for `catenary sed` preview / write-summary overflow spill
/// files.
pub const SED_PREFIX: &str = "sed-";

/// Most recent query (`grep-*`/`glob-*`/`sed-*`) spill files a single daemon
/// retains *per prefix*.
///
/// Each valve overflow mints a fresh UUID, so the files never overwrite; without
/// a cap a long-running daemon that truncates many large queries would grow the
/// dir unbounded between restarts.
pub const MAX_QUERY_OVERFLOW_FILES: usize = 8;

/// The directory holding overflow report files under `base`.
fn dir(base: &Path) -> PathBuf {
    base.join("catenary")
}

/// Extracts the scope key from a report filename given its prefix
/// (`diagnostics-<key>.txt` → `<key>`). Returns `None` for non-matching names.
fn key_of<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    name.strip_prefix(prefix)?.strip_suffix(SUFFIX)
}

/// Removes report files with `prefix` whose key fails `keep`, returning the count
/// removed. A missing overflow directory is not an error (nothing to sweep).
fn sweep(base: &Path, prefix: &str, keep: impl Fn(&str) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir(base)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(key) = name.to_str().and_then(|n| key_of(n, prefix)) else {
            continue;
        };
        if !keep(key) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

// ── Diagnostics: per-session, one-shot ──────────────────────────────────────

/// Path to a session's diagnostics overflow file under `base`.
///
/// Stable per session (`diagnostics-<session_id>.txt`), so each run overwrites
/// the previous one — at most one file per session.
#[must_use]
pub fn diagnostics_path(base: &Path, session_id: &str) -> PathBuf {
    dir(base).join(format!("{DIAGNOSTICS_PREFIX}{session_id}{SUFFIX}"))
}

/// Write the complete diagnostics report to a session's overflow file,
/// overwriting any previous run. Returns the path written.
///
/// Catenary writes this itself (it owns the full set and the session id), so it
/// never passes through the host's edit tool or a shell redirect.
///
/// # Errors
///
/// Returns an error if the runtime directory cannot be created or the file cannot
/// be written.
pub fn write_diagnostics(
    base: &Path,
    session_id: &str,
    contents: &str,
) -> std::io::Result<PathBuf> {
    let d = dir(base);
    std::fs::create_dir_all(&d)?;
    let path = d.join(format!("{DIAGNOSTICS_PREFIX}{session_id}{SUFFIX}"));
    std::fs::write(&path, contents)?;
    Ok(path)
}

/// Best-effort removal of a session's diagnostics overflow file (e.g. on session
/// end).
pub fn remove_diagnostics(base: &Path, session_id: &str) {
    let _ = std::fs::remove_file(diagnostics_path(base, session_id));
}

/// Remove diagnostics overflow files whose session id is not in `live_ids`.
///
/// The lazy GC the diagnostics design specifies: it rides the session prune
/// (daemon startup) to clear crash leftovers and files from ended sessions.
/// Returns the number of files removed. A missing overflow directory is not an
/// error (nothing to sweep).
#[must_use]
pub fn sweep_diagnostics<S: BuildHasher>(base: &Path, live_ids: &HashSet<String, S>) -> usize {
    sweep(base, DIAGNOSTICS_PREFIX, |key| live_ids.contains(key))
}

/// Enforce the in-lifetime last-`max` cap on `<prefix>*.txt` reports, removing
/// the oldest beyond `max` by modification time. Best-effort. Shared by the
/// grep/glob/sed valve ([`GREP_PREFIX`]/[`GLOB_PREFIX`]/[`SED_PREFIX`],
/// [`MAX_QUERY_OVERFLOW_FILES`]).
fn enforce_cap(base: &Path, prefix: &str, max: usize) {
    let Ok(entries) = std::fs::read_dir(dir(base)) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| key_of(n, prefix).is_some())
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    if files.len() <= max {
        return;
    }
    // Newest first, then drop everything past the cap.
    files.sort_unstable_by_key(|f| std::cmp::Reverse(f.0));
    for (_, path) in files.into_iter().skip(max) {
        let _ = std::fs::remove_file(path);
    }
}

// ── Query valve: grep / glob / sed, per-invocation truncate-and-spill ────────

/// Outcome of applying the overflow [`valve`] to a query's rendered output.
pub struct Valved {
    /// The output to print to stdout: `full` verbatim when within the line
    /// budget, truncated to the budget (at a block boundary for glob/sed)
    /// otherwise.
    pub display: String,
    /// `Some(receipt)` when truncation spilled the complete output to a file —
    /// a one-line stderr notice naming the line count and spill path. `None`
    /// when the output fit the budget, or when the spill write failed and the
    /// full output is emitted instead (mirrors diagnostics: never lose output).
    pub receipt: Option<String>,
}

/// Writes the complete query output to a fresh per-invocation spill file
/// `<base>/catenary/<prefix><uuid>.txt`, enforces the in-lifetime cap, and
/// returns the path.
///
/// A fresh UUID per invocation means spills never overwrite (the query is a
/// stateless grep-class request with no session to key on), so
/// [`enforce_cap`] bounds the dir between daemon restarts and
/// [`sweep_query`] clears leftovers at startup.
fn write_query_overflow(base: &Path, prefix: &str, full: &str) -> std::io::Result<PathBuf> {
    let d = dir(base);
    std::fs::create_dir_all(&d)?;
    let path = d.join(format!("{prefix}{}{SUFFIX}", uuid::Uuid::new_v4()));
    std::fs::write(&path, full)?;
    enforce_cap(base, prefix, MAX_QUERY_OVERFLOW_FILES);
    Ok(path)
}

/// Remove every `grep-*`/`glob-*`/`sed-*` valve spill file under `base`. Returns
/// the count removed. A missing overflow directory is not an error (nothing to
/// sweep).
///
/// Run at daemon startup: valve spills are per-invocation (a fresh UUID, never
/// overwritten) and unreferenced by any session, so a previous daemon's
/// leftovers are all reaped. Self-contained — no live-session set needed.
#[must_use]
pub fn sweep_query(base: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir(base)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if entry.file_name().to_str().is_some_and(|n| {
            n.starts_with(GREP_PREFIX) || n.starts_with(GLOB_PREFIX) || n.starts_with(SED_PREFIX)
        }) && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Bounds a query's display at `budget_lines`, spilling the **complete** output
/// to a runtime-dir file and returning a stderr receipt when it truncates.
///
/// Within budget (`total <= budget_lines`) the output passes through untouched
/// with no receipt. Over budget, the display is cut to `budget_lines` and the
/// full output is written to `<base>/catenary/<prefix><uuid>.txt`; the receipt
/// points at it. If the spill write fails the full output is emitted with no
/// receipt — nothing is ever lost (mirrors the diagnostics overflow path).
///
/// `boundary` backs the cut up to the last line that *begins* a complete
/// top-level block, so a per-unit block is never severed mid-way: callers with
/// block-structured output pass `Some(predicate)` (the first dropped line must
/// start a fresh block — glob's outline tree, sed's per-file diff), while
/// callers with self-contained lines pass `None` (a hard line cut — grep's
/// match lines, sed's `--in-place` file list). A single block larger than the
/// whole budget falls back to a hard cut.
#[must_use]
pub fn valve(
    full: &str,
    budget_lines: usize,
    base: &Path,
    prefix: &str,
    boundary: Option<&dyn Fn(&str) -> bool>,
) -> Valved {
    let lines: Vec<&str> = full.lines().collect();
    let total = lines.len();
    if total <= budget_lines {
        return Valved {
            display: full.to_string(),
            receipt: None,
        };
    }
    let cut = boundary.map_or(budget_lines, |is_boundary| {
        // The first DROPPED line (`lines[cut]`) must begin a fresh block.
        let mut k = budget_lines;
        while k > 1 && !is_boundary(lines[k]) {
            k -= 1;
        }
        if is_boundary(lines[k]) {
            k
        } else {
            // A single block spans past the budget — hard cut.
            budget_lines
        }
    });
    let display: String = lines[..cut].iter().flat_map(|l| [*l, "\n"]).collect();
    write_query_overflow(base, prefix, full).map_or_else(
        // Spill failed — emit the full output, lose nothing (mirrors diagnostics).
        |_| Valved {
            display: full.to_string(),
            receipt: None,
        },
        |path| Valved {
            display,
            receipt: Some(format!(
                "output truncated to protect context; full output ({total} lines) at {}",
                path.display()
            )),
        },
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Diagnostics (per-session, one-shot) ─────────────────────────

    #[test]
    fn diagnostics_written_and_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        let p1 = write_diagnostics(base, "sess-1", "first").expect("write");
        assert_eq!(p1, diagnostics_path(base, "sess-1"));
        assert_eq!(std::fs::read_to_string(&p1).expect("read"), "first");
        // A second run overwrites rather than appends.
        let p2 = write_diagnostics(base, "sess-1", "second").expect("write");
        assert_eq!(p1, p2);
        assert_eq!(std::fs::read_to_string(&p2).expect("read"), "second");
    }

    #[test]
    fn sweep_diagnostics_removes_orphans() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        write_diagnostics(base, "live-1", "x").expect("write");
        write_diagnostics(base, "dead-2", "x").expect("write");
        write_diagnostics(base, "orphan-3", "x").expect("write");
        let live: HashSet<String> = std::iter::once("live-1".to_string()).collect();
        let removed = sweep_diagnostics(base, &live);
        assert_eq!(removed, 2);
        assert!(diagnostics_path(base, "live-1").exists());
        assert!(!diagnostics_path(base, "dead-2").exists());
        assert!(!diagnostics_path(base, "orphan-3").exists());
    }

    #[test]
    fn sweep_diagnostics_missing_dir_is_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(sweep_diagnostics(dir.path(), &HashSet::new()), 0);
    }

    #[test]
    fn remove_diagnostics_deletes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        write_diagnostics(base, "s", "x").expect("write");
        assert!(diagnostics_path(base, "s").exists());
        remove_diagnostics(base, "s");
        assert!(!diagnostics_path(base, "s").exists());
    }

    // ── Query valve (grep / glob / sed, per-invocation truncate-and-spill) ─

    #[test]
    fn valve_within_budget_passes_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let full = "a\nb\nc\n";
        let v = valve(full, 10, dir.path(), GREP_PREFIX, None);
        assert_eq!(v.display, full);
        assert!(v.receipt.is_none(), "no truncation → no receipt");
        assert_eq!(sweep_query(dir.path()), 0, "no spill file written");
    }

    #[test]
    fn valve_hard_cut_truncates_and_spills_full_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        // 5 lines, budget 2 → display holds 2 lines, full output spilled.
        let full = "l0\nl1\nl2\nl3\nl4";
        let v = valve(full, 2, base, GREP_PREFIX, None);
        assert_eq!(
            v.display, "l0\nl1\n",
            "hard cut at the budget line: {:?}",
            v.display
        );
        let receipt = v.receipt.expect("truncation → receipt");
        assert!(
            receipt.contains("full output (5 lines) at "),
            "receipt names the total line count + path: {receipt}"
        );
        // The spill file holds the COMPLETE output.
        let path = receipt
            .rsplit(" at ")
            .next()
            .map(std::path::PathBuf::from)
            .expect("path in receipt");
        assert_eq!(std::fs::read_to_string(&path).expect("read spill"), full);
    }

    #[test]
    fn valve_boundary_backs_up_to_block_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        // Two "files": header `f` then two `:` detail lines each. A non-`:` line
        // is a block boundary (the glob predicate). Budget 4 would cut mid-block
        // (the first dropped line `:b1` is NOT a boundary), so the valve backs
        // the cut up to the second header at index 3.
        let full = "fa\n:a0\n:a1\nfb\n:b0\n:b1";
        let is_boundary = |l: &str| !l.trim_start().starts_with(':');
        let v = valve(full, 4, base, GLOB_PREFIX, Some(&is_boundary));
        assert_eq!(
            v.display, "fa\n:a0\n:a1\n",
            "cut backed up to the start of the second file's block: {:?}",
            v.display
        );
        assert!(v.receipt.is_some(), "truncation → receipt");
    }

    #[test]
    fn sweep_query_removes_grep_glob_and_sed_spills() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        // Force a spill for each prefix.
        let _ = valve("x\ny\nz", 1, base, GREP_PREFIX, None);
        let _ = valve("x\ny\nz", 1, base, GLOB_PREFIX, None);
        let _ = valve("x\ny\nz", 1, base, SED_PREFIX, None);
        // A diagnostics file must survive the query sweep (cross-scope safety).
        write_diagnostics(base, "sess", "diag").expect("write diag");
        assert_eq!(sweep_query(base), 3, "every valve spill reaped");
        assert!(
            diagnostics_path(base, "sess").exists(),
            "query sweep leaves diagnostics files alone"
        );
    }

    #[test]
    fn valve_in_lifetime_cap_bounds_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        // Force more spills than the per-prefix cap; each valve call enforces it.
        for _ in 0..(MAX_QUERY_OVERFLOW_FILES + 5) {
            let _ = valve("x\ny\nz", 1, base, SED_PREFIX, None);
        }
        let count = std::fs::read_dir(super::dir(base))
            .expect("read dir")
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| key_of(n, SED_PREFIX).is_some())
            })
            .count();
        assert!(
            count <= MAX_QUERY_OVERFLOW_FILES,
            "in-lifetime cap keeps at most {MAX_QUERY_OVERFLOW_FILES}, found {count}"
        );
    }
}
