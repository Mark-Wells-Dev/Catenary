// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The CLI-owns-the-walk engine skeleton (ws43).
//!
//! The CLI walks and matches — the ripgrep engine (`grep-regex` +
//! `grep-searcher` + `ignore`) the daemon-less twin already links — and emits
//! **ordered** hit-batches. The walk is the ingestion seam where a hit path
//! becomes canonical ([`super::canonicalize_hit_path`]), so every hit that
//! crosses the wire carries a canonical path.
//!
//! This is a skeleton: it is a straight, single-threaded walk in the walk's
//! natural (path-sorted, line-ordered) order, sufficient to feed the sinks and
//! prove ordered emission. The production query path's parallel walk, binary
//! skips, and `--type`/`-g` filters are the current query engine's job; the
//! cutover that folds them into this seam is a later ticket.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};

use super::{HIT_BATCH_SIZE, WireHit, canonicalize_hit_path};

/// A single grep hit produced by the walk, before it is batched onto the wire.
///
/// A thin re-export of [`WireHit`] under the engine's name — the walk emits wire
/// hits directly (canonical path, 1-based line/column, verbatim text), so there
/// is no separate in-memory hit type to keep in sync.
pub type Hit = WireHit;

/// An ordered batch of hits with its batch sequence number.
///
/// The walk emits these in strict `seq` order (0-based, gap-free). A batch holds
/// at most [`HIT_BATCH_SIZE`] hits; the last batch may be short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitBatch {
    /// Monotonic batch sequence number, 0-based.
    pub seq: u64,
    /// The hits in this batch, in the walk's global order.
    pub hits: Vec<Hit>,
}

/// The subset of ripgrep-parity knobs the skeleton walk honors.
///
/// Deliberately minimal — the skeleton proves the seam, not flag parity. The
/// cutover ticket widens this to the full `GrepFlags` surface, so the flag set is
/// a stand-in that will be replaced wholesale rather than grown here.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a stand-in for GrepFlags; the cutover ticket replaces it with the real flag struct"
)]
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Case-insensitive match (`-i`).
    pub ignore_case: bool,
    /// Match only whole words (`-w`).
    pub word: bool,
    /// Treat the pattern as a literal string (`-F`).
    pub fixed_strings: bool,
    /// Walk gitignored files too (default: skip them).
    pub include_gitignored: bool,
    /// Walk hidden files/dirs too (default: skip them).
    pub include_hidden: bool,
}

/// Walks `roots` for `pattern`, emitting each ordered [`HitBatch`] to `on_batch`.
///
/// The walk visits files in the `ignore` walker's order (path-sorted within a
/// root) and matches each line with the same `grep-*` engine the query path
/// uses. Hits accumulate into batches of at most [`HIT_BATCH_SIZE`]; a full
/// batch is handed to `on_batch` immediately (nothing is buffered into a
/// whole-result set), and a final short batch flushes at the end. `on_batch`
/// receives batches in strict `seq` order.
///
/// Every hit path is canonicalized at this seam ([`canonicalize_hit_path`]).
///
/// Returns the number of batches emitted.
///
/// # Errors
///
/// Returns an error if the pattern is not a valid regex or a root cannot be
/// walked. A per-file read error is skipped (logged at debug), never fatal — a
/// walk degrades to fewer hits from an unreadable file, never to no results.
pub fn walk<F>(
    pattern: &str,
    roots: &[PathBuf],
    options: &WalkOptions,
    mut on_batch: F,
) -> Result<u64>
where
    F: FnMut(HitBatch) -> Result<()>,
{
    let matcher = build_matcher(pattern, options)?;
    let mut batcher = Batcher::new(HIT_BATCH_SIZE, &mut on_batch);

    for root in roots {
        walk_root(root, &matcher, options, &mut batcher)?;
    }

    batcher.finish()
}

/// Builds the line matcher from the pattern and the skeleton's flag subset —
/// the same `grep-regex` builder the query path's `build_matcher` uses.
fn build_matcher(pattern: &str, options: &WalkOptions) -> Result<grep_regex::RegexMatcher> {
    let effective = if options.fixed_strings {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let mut builder = RegexMatcherBuilder::new();
    if options.ignore_case {
        builder.case_insensitive(true);
    } else {
        builder.case_smart(true);
    }
    builder.word(options.word);
    builder
        .build(&effective)
        .map_err(|e| anyhow!("Invalid regex pattern: {e}"))
}

/// Builds a line searcher with line numbers on (the hit line/column format needs
/// them).
fn build_searcher() -> Searcher {
    SearcherBuilder::new().line_number(true).build()
}

/// Walks one root, feeding every match into `batcher` in walk order.
fn walk_root<F>(
    root: &Path,
    matcher: &grep_regex::RegexMatcher,
    options: &WalkOptions,
    batcher: &mut Batcher<'_, F>,
) -> Result<()>
where
    F: FnMut(HitBatch) -> Result<()>,
{
    use ignore::WalkBuilder;

    // A named file bypasses the gitignore/hidden gate — those govern recursive
    // directory traversal, not a path the caller named (misc 110, ripgrep
    // parity). Mirrors the query engine's `root_is_file` bypass.
    let root_is_file = root.is_file();
    let skip_gitignored = !options.include_gitignored && !root_is_file;
    let skip_hidden = !options.include_hidden && !root_is_file;

    let walker = WalkBuilder::new(root)
        .git_ignore(skip_gitignored)
        .hidden(skip_hidden)
        .sort_by_file_path(std::cmp::Ord::cmp)
        .build();

    let mut searcher = build_searcher();

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::debug!("hitstream walk: skipping entry: {e}");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        // Canonicalize at the ingestion seam — the hit carries a canonical path.
        let canonical = canonicalize_hit_path(path);

        let mut sink = HitSink {
            matcher,
            path: &canonical,
            batcher,
            error: None,
        };
        if let Err(e) = searcher.search_path(matcher, path, &mut sink) {
            // A per-file read/search error is a skip, never fatal — degrade to
            // fewer hits, never to no results.
            tracing::debug!("hitstream walk: skipping {}: {e}", path.display());
        }
        // Surface a batching (sink-side callback) error: it is the caller's
        // `on_batch` failing, which must abort the walk, not be swallowed.
        if let Some(err) = sink.error {
            return Err(err);
        }
    }
    Ok(())
}

/// Accumulates hits into fixed-size batches, handing each full batch to the
/// caller's `on_batch` in strict `seq` order. Nothing is buffered beyond one
/// in-progress batch — the streaming, no-buffered-result-set invariant.
struct Batcher<'a, F> {
    cap: usize,
    seq: u64,
    current: Vec<Hit>,
    on_batch: &'a mut F,
}

impl<'a, F> Batcher<'a, F>
where
    F: FnMut(HitBatch) -> Result<()>,
{
    fn new(cap: usize, on_batch: &'a mut F) -> Self {
        Self {
            cap: cap.max(1),
            seq: 0,
            current: Vec::with_capacity(cap.max(1)),
            on_batch,
        }
    }

    /// Adds one hit, flushing a full batch to `on_batch`.
    fn push(&mut self, hit: Hit) -> Result<()> {
        self.current.push(hit);
        if self.current.len() >= self.cap {
            self.flush()?;
        }
        Ok(())
    }

    /// Emits the in-progress batch (if non-empty) and advances `seq`.
    fn flush(&mut self) -> Result<()> {
        if self.current.is_empty() {
            return Ok(());
        }
        let hits = std::mem::take(&mut self.current);
        let batch = HitBatch {
            seq: self.seq,
            hits,
        };
        self.seq += 1;
        (self.on_batch)(batch)
    }

    /// Flushes any partial final batch and returns the total batch count.
    fn finish(mut self) -> Result<u64> {
        self.flush()?;
        Ok(self.seq)
    }
}

/// A `grep-searcher` sink that turns each match into a [`Hit`] and pushes it
/// onto the batcher. A batcher-callback error is captured in `error` (the
/// searcher `Sink` trait's error type is I/O-shaped, so a caller-callback error
/// is stashed and re-surfaced by the walk).
struct HitSink<'a, 'b, F> {
    matcher: &'a grep_regex::RegexMatcher,
    path: &'a Path,
    batcher: &'a mut Batcher<'b, F>,
    error: Option<anyhow::Error>,
}

impl<F> Sink for HitSink<'_, '_, F>
where
    F: FnMut(HitBatch) -> Result<()>,
{
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> std::result::Result<bool, std::io::Error> {
        // Already surfacing a callback error — stop the search for this file.
        if self.error.is_some() {
            return Ok(false);
        }

        let line = mat.line_number().unwrap_or(0);
        let text = String::from_utf8_lossy(mat.bytes())
            .trim_end_matches(['\n', '\r'])
            .to_string();

        // 1-based column of the first match on the line (ripgrep display
        // convention). `find_at` on the line bytes locates it; a zero-width or
        // failed find falls back to column 1.
        let column = self
            .matcher
            .find_at(mat.bytes(), 0)
            .ok()
            .flatten()
            .map_or(1, |m| {
                u32::try_from(m.start()).unwrap_or(0).saturating_add(1)
            });

        let hit = Hit {
            path: self.path.to_path_buf(),
            line: u32::try_from(line).unwrap_or(0),
            column,
            text,
        };

        if let Err(e) = self.batcher.push(hit) {
            self.error = Some(e);
            return Ok(false);
        }
        Ok(true)
    }
}

/// Walks `roots` for `pattern`, collecting every batch into an ordered vector.
///
/// A convenience over [`walk`] for the stdout sink and tests: it materializes the
/// batches, but the walk itself still streams (each batch is produced and pushed
/// as the walk finds it). Callers that must not buffer — the daemon-stream sink —
/// use [`walk`] with a streaming callback instead.
///
/// # Errors
///
/// Returns an error if the walk fails (bad pattern, unwalkable root).
pub fn collect_batches(
    pattern: &str,
    roots: &[PathBuf],
    options: &WalkOptions,
) -> Result<Vec<HitBatch>> {
    let mut batches = Vec::new();
    walk(pattern, roots, options, |batch| {
        batches.push(batch);
        Ok(())
    })
    .context("collect walk batches")?;
    Ok(batches)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    reason = "tests use expect for readable assertions and small fixture counts"
)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_file(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let mut f = std::fs::File::create(&path).expect("create file");
        f.write_all(body.as_bytes()).expect("write file");
    }

    #[test]
    fn walk_emits_ordered_gap_free_batches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Enough matches to force several batches.
        let count = HIT_BATCH_SIZE * 2 + 5;
        let mut body = String::new();
        for _ in 0..count {
            body.push_str("needle here\n");
        }
        write_file(root, "a.txt", &body);

        let batches = collect_batches("needle", &[root.to_path_buf()], &WalkOptions::default())
            .expect("walk");

        // seq is 0-based and gap-free.
        for (i, batch) in batches.iter().enumerate() {
            assert_eq!(batch.seq, i as u64, "batch {i} has seq {}", batch.seq);
        }
        let total: usize = batches.iter().map(|b| b.hits.len()).sum();
        assert_eq!(total, count, "every match is emitted exactly once");
        // Every batch but the last is full.
        for batch in &batches[..batches.len() - 1] {
            assert_eq!(batch.hits.len(), HIT_BATCH_SIZE);
        }
    }

    #[test]
    fn walk_canonicalizes_hit_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_file(root, "sub/b.txt", "match me\n");

        // Walk through the un-canonicalized (possibly symlinked on macOS)
        // tempdir path; the hit path must come back canonical.
        let batches =
            collect_batches("match", &[root.to_path_buf()], &WalkOptions::default()).expect("walk");
        let hit = &batches[0].hits[0];
        let canonical = root.join("sub/b.txt").canonicalize().expect("canonicalize");
        assert_eq!(hit.path, canonical, "hit path is canonical");
    }

    #[test]
    fn walk_records_line_and_column() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_file(root, "c.txt", "aaa\nxx needle yy\n");

        let batches = collect_batches("needle", &[root.to_path_buf()], &WalkOptions::default())
            .expect("walk");
        let hit = &batches[0].hits[0];
        assert_eq!(hit.line, 2, "1-based line");
        assert_eq!(hit.column, 4, "1-based column of the first match");
        assert_eq!(hit.text, "xx needle yy");
    }

    #[test]
    fn empty_walk_emits_no_batches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_file(root, "d.txt", "nothing to see\n");

        let batches = collect_batches("absent", &[root.to_path_buf()], &WalkOptions::default())
            .expect("walk");
        assert!(batches.is_empty(), "no matches, no batches");
    }

    #[test]
    fn bad_pattern_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = collect_batches(
            "(unclosed",
            &[tmp.path().to_path_buf()],
            &WalkOptions::default(),
        );
        assert!(err.is_err(), "an uncompilable pattern errors");
    }

    #[test]
    fn symlink_alias_root_yields_canonical_hit_path() {
        // Path-spelling discipline: walking through a symlinked alias of the
        // real root must canonicalize the hit path to the real target. This is
        // the class Linux-green does not prove (misc 193 lesson) — pin it here.
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).expect("mkdir real");
        write_file(&real, "e.txt", "aliased match\n");

        let alias = tmp.path().join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");
        #[cfg(not(unix))]
        return; // symlink alias test is unix-only

        let batches = collect_batches(
            "aliased",
            std::slice::from_ref(&alias),
            &WalkOptions::default(),
        )
        .expect("walk");
        let hit = &batches[0].hits[0];
        let expected = real
            .join("e.txt")
            .canonicalize()
            .expect("canonicalize real");
        assert_eq!(
            hit.path, expected,
            "a hit found through a symlink alias carries the canonical real path"
        );
        assert!(
            !hit.path.starts_with(&alias),
            "the alias spelling never leaks into the hit path"
        );
    }
}
