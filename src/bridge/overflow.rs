// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Runtime-dir overflow reports shared by two truncating surfaces.
//!
//! `catenary diagnostics` (cli-prerelease ticket 11) and `catenary sed`'s preview
//! (ticket 11a) both bound their stdout in memory and spill the *complete* output
//! to a file under `<runtime_dir>/catenary/` when that bound is hit, ending the
//! truncated preview with a `… full … at <path>` pointer so the agent can read or
//! `catenary grep` the dropped tail. This module owns the one mechanism; the two
//! surfaces differ only in:
//!
//! - **scope key** — diagnostics names its file per live session
//!   (`diagnostics-<session_id>.txt`, overwritten each run); sed mints a fresh
//!   per-invocation UUID (`sed-<uuid>.txt`, never overwritten) because the preview
//!   is a stateless query (grep-class) with no session to key on.
//! - **write shape** — diagnostics renders its (small, one-turn) set up front and
//!   writes it in one shot ([`write_diagnostics`]); a sed sweep can be enormous, so
//!   [`SedOverflowWriter`] *streams* each file's diff as it is computed and never
//!   assembles the whole diff in memory (preserving the preview's no-OOM
//!   guarantee).
//! - **GC** — diagnostics rides the session prune ([`sweep_diagnostics`] removes
//!   files whose id is no longer alive); sed has no session, so a daemon-startup
//!   [`sweep_sed`] clears every leftover and an in-lifetime last-N cap
//!   ([`MAX_SED_OVERFLOW_FILES`]) bounds a long-running daemon between restarts.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Filename prefix for per-session diagnostics overflow reports.
const DIAGNOSTICS_PREFIX: &str = "diagnostics-";
/// Filename prefix for per-invocation sed preview overflow reports (and their
/// in-flight temp files).
const SED_PREFIX: &str = "sed-";
/// `.txt` suffix shared by both report kinds.
const SUFFIX: &str = ".txt";

/// Most recent `sed-*.txt` previews a single daemon retains.
///
/// A fresh UUID per invocation means these never overwrite, so without a cap a
/// long-running daemon that runs many previews would grow the dir unbounded
/// between restarts.
pub const MAX_SED_OVERFLOW_FILES: usize = 8;

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

// ── Sed preview: per-invocation, streamed ───────────────────────────────────

/// Path to a sed preview's overflow file under `base` (`sed-<id>.txt`).
#[must_use]
pub fn sed_path(base: &Path, id: &str) -> PathBuf {
    dir(base).join(format!("{SED_PREFIX}{id}{SUFFIX}"))
}

/// Remove every `sed-*` preview overflow file (persisted reports *and* any stray
/// in-flight temp left by a crash). Returns the count removed.
///
/// Run at daemon startup: a previous daemon's per-invocation previews are
/// unreferenced (no session prune reclaims them, and each invocation mints a fresh
/// UUID), so they are all reaped. Self-contained — no live-session set needed.
#[must_use]
pub fn sweep_sed(base: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir(base)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(SED_PREFIX))
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Enforce the in-lifetime last-N cap on `sed-*.txt` reports, removing the oldest
/// beyond [`MAX_SED_OVERFLOW_FILES`] by modification time. Best-effort.
fn enforce_sed_cap(base: &Path) {
    let Ok(entries) = std::fs::read_dir(dir(base)) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| key_of(n, SED_PREFIX).is_some())
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    if files.len() <= MAX_SED_OVERFLOW_FILES {
        return;
    }
    // Newest first, then drop everything past the cap.
    files.sort_unstable_by_key(|f| std::cmp::Reverse(f.0));
    for (_, path) in files.into_iter().skip(MAX_SED_OVERFLOW_FILES) {
        let _ = std::fs::remove_file(path);
    }
}

/// Streams a sed preview's full, uncapped diff to a runtime-dir overflow file.
///
/// Created with the runtime-dir base and the daemon-minted per-invocation UUID,
/// but opens nothing until the first diff is appended — a preview that renders
/// entirely in memory leaves no file. Each file's diff is written as it is
/// computed (see [`append`](Self::append)), so the whole diff is never assembled
/// in memory and a repo-wide sweep cannot OOM the daemon. [`finish`](Self::finish)
/// persists the file as `sed-<uuid>.txt` only when the preview truncated; an
/// untruncated preview discards the temp file, leaving no `sed-*.txt` behind.
pub struct SedOverflowWriter {
    /// Runtime-dir base (the overflow dir is `<base>/catenary/`).
    base: PathBuf,
    /// Per-invocation UUID naming the persisted file.
    id: String,
    /// Lazily-opened temp file in the overflow dir; `None` until the first append.
    file: Option<tempfile::NamedTempFile>,
    /// Set once an IO error is hit so we stop trying and degrade to no overflow
    /// file (the preview still shows its in-memory truncation summary).
    failed: bool,
}

impl SedOverflowWriter {
    /// Creates a writer for the per-invocation UUID `id` under runtime-dir `base`.
    /// No file is created until the first [`append`](Self::append).
    #[must_use]
    pub fn new(base: impl Into<PathBuf>, id: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            id: id.into(),
            file: None,
            failed: false,
        }
    }

    /// Lazily creates the temp file in the overflow dir, returning a handle to it.
    /// Returns `None` once an IO error has disabled the writer.
    fn ensure_file(&mut self) -> Option<&mut tempfile::NamedTempFile> {
        if self.failed {
            return None;
        }
        if self.file.is_none() {
            let d = dir(&self.base);
            if std::fs::create_dir_all(&d).is_err() {
                self.failed = true;
                return None;
            }
            if let Ok(f) = tempfile::Builder::new()
                .prefix(SED_PREFIX)
                .suffix(".tmp")
                .tempfile_in(&d)
            {
                self.file = Some(f);
            } else {
                self.failed = true;
                return None;
            }
        }
        self.file.as_mut()
    }

    /// Appends one pre-rendered diff section (one file's complete, uncapped diff),
    /// streaming it straight to disk. An empty section is ignored (so a no-op file
    /// never forces the temp file open); an IO error disables further writes.
    pub fn append(&mut self, section: &str) {
        if section.is_empty() {
            return;
        }
        let Some(file) = self.ensure_file() else {
            return;
        };
        if file.write_all(section.as_bytes()).is_err() {
            self.failed = true;
        }
    }

    /// Finalizes the report: when `truncated` (and at least one section was
    /// written without error), persist the temp file as `sed-<uuid>.txt`, enforce
    /// the in-lifetime cap, and return the path. Otherwise discard the temp file
    /// and return `None` — no `sed-*.txt` is left behind.
    #[must_use]
    pub fn finish(mut self, truncated: bool) -> Option<PathBuf> {
        let file = self.file.take()?;
        if !truncated || self.failed {
            // Dropping `file` removes the temp file.
            return None;
        }
        let path = sed_path(&self.base, &self.id);
        let persisted = file.persist(&path).is_ok();
        if persisted {
            enforce_sed_cap(&self.base);
            Some(path)
        } else {
            None
        }
    }
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

    /// The two scopes share a directory but never collide: a diagnostics sweep
    /// leaves `sed-*` alone and the sed sweep leaves `diagnostics-*` alone.
    #[test]
    fn sweeps_do_not_cross_scopes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        write_diagnostics(base, "sess-1", "diag").expect("write");
        let w = {
            let mut w = SedOverflowWriter::new(base, "abc");
            w.append("a sed section\n");
            w
        };
        let sed_file = w.finish(true).expect("persist");

        // A diagnostics sweep (no live sessions) removes the diag file only.
        assert_eq!(sweep_diagnostics(base, &HashSet::new()), 1);
        assert!(sed_file.exists(), "sed file untouched by diagnostics sweep");
        assert!(!diagnostics_path(base, "sess-1").exists());

        // A sed sweep removes the sed file only.
        write_diagnostics(base, "sess-2", "diag2").expect("write");
        assert_eq!(sweep_sed(base), 1);
        assert!(!sed_file.exists());
        assert!(diagnostics_path(base, "sess-2").exists());
    }

    // ── Sed preview (per-invocation, streamed) ──────────────────────

    #[test]
    fn sed_writer_no_truncation_leaves_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        let mut w = SedOverflowWriter::new(base, "id-1");
        w.append("some diff\n");
        // Untruncated → discard, no sed-*.txt.
        assert_eq!(w.finish(false), None);
        assert!(!sed_path(base, "id-1").exists());
        assert_eq!(sweep_sed(base), 0, "no stray temp left behind");
    }

    #[test]
    fn sed_writer_truncation_persists_streamed_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        let mut w = SedOverflowWriter::new(base, "id-2");
        w.append("section-a\n");
        w.append("section-b\n");
        let path = w.finish(true).expect("persist on truncation");
        assert_eq!(path, sed_path(base, "id-2"));
        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(
            contents.contains("section-a") && contents.contains("section-b"),
            "streamed sections survive: {contents}"
        );
    }

    #[test]
    fn sed_writer_empty_never_opens_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        let mut w = SedOverflowWriter::new(base, "id-3");
        // No append (or only empty sections) → finish has nothing to persist.
        w.append("");
        assert_eq!(w.finish(true), None);
        assert!(!sed_path(base, "id-3").exists());
    }

    #[test]
    fn sweep_sed_removes_all_previews() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        for id in ["a", "b", "c"] {
            let mut w = SedOverflowWriter::new(base, id);
            w.append("x\n");
            w.finish(true).expect("persist");
        }
        assert_eq!(sweep_sed(base), 3);
        for id in ["a", "b", "c"] {
            assert!(!sed_path(base, id).exists());
        }
    }

    #[test]
    fn sed_in_lifetime_cap_bounds_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        // Persist more than the cap; finish() enforces it each time.
        for i in 0..(MAX_SED_OVERFLOW_FILES + 5) {
            let mut w = SedOverflowWriter::new(base, format!("id-{i:03}"));
            w.append("x\n");
            w.finish(true).expect("persist");
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
            count <= MAX_SED_OVERFLOW_FILES,
            "in-lifetime cap keeps at most {MAX_SED_OVERFLOW_FILES}, found {count}"
        );
    }
}
