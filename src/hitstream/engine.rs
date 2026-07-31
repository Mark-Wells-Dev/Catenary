// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The CLI-owns-the-walk engine (ws43).
//!
//! The CLI walks and matches — the ripgrep engine (`grep-regex` +
//! `grep-searcher` + `ignore`) the daemon-less twin already links — and emits
//! **ordered** hit-batches. The walk is the ingestion seam where a hit path
//! becomes canonical ([`super::canonicalize_hit_path`]), so every hit that
//! crosses the wire carries a canonical path.
//!
//! Since ws43-02 the walk covers the full `catenary grep` flag surface: the
//! matcher modifiers (`-i`/`-s`/`-w`/`-F`) and line-selection modifiers (`-v`,
//! context `-A`/`-B`/`-C`) ride [`GrepFlags`] through the executor's own
//! matcher/searcher constructors (shared, so the two walks cannot drift), the
//! positive file filters (`-g`/`--type`) and the `--exclude-pattern` set gate
//! the directory traversal, and binary files are skipped-and-recorded exactly
//! as the query engine records them ([`SkipRecord`], misc 135). `--count` and
//! `-l` need no engine mode of their own: with context flags cleared every hit
//! is a match line, so a tally (matching lines / distinct files) falls out of
//! the same walk.
//!
//! The walk is a single-threaded, path-sorted traversal: emission order is the
//! deterministic global `(file, line)` order the query path renders, which is
//! what lets the sinks stream without a global sort buffer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use grep_matcher::Matcher;
use grep_searcher::{Searcher, Sink, SinkContext, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::types::{Types, TypesBuilder};

use crate::bridge::filesystem_manager::{
    FilesystemManager, OBSERVED_STAT_MISS_MTIME, mtime_nanos, stat_with_retry,
};
use crate::bridge::session::ExcludeSet;
use crate::bridge::{GrepFlags, SkipRecord};

use super::{HIT_BATCH_SIZE, WireHit, canonicalize_hit_path};

/// A single grep hit produced by the walk, before it is batched onto the wire.
///
/// A thin re-export of [`WireHit`] under the engine's name — the walk emits wire
/// hits directly (canonical path, 1-based line, verbatim text), so there is no
/// separate in-memory hit type to keep in sync.
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
    /// The WS31 observations recorded since the previous flush: every regular
    /// file the walk visited (matched or not), `(path, mtime-nanos)`. Riding
    /// the batch keeps the daemon's observation nudge ordered before the
    /// batch's anchors — and keeps a cold root's first nudge the cold snapshot
    /// (first-walk `Changed`), the executor's classification. A visited file
    /// whose stat misses carries the `OBSERVED_STAT_MISS_MTIME` sentinel,
    /// never omission (WS31-review H1).
    pub observed: Vec<(PathBuf, i64)>,
}

/// The knobs the walk honors — the full `catenary grep` flag surface (ws43-02).
///
/// `flags` is the real [`GrepFlags`] struct the CLI, the IPC request, and the
/// query executor share, so a flag lands here without translation. The exclude
/// set and the scope toggles ride alongside exactly as `GrepInput` carries
/// them.
#[derive(Clone, Default)]
pub struct WalkOptions {
    /// Ripgrep-parity flags: case/word/fixed/invert/context/glob/type. The
    /// files-with-matches flag does not change the walk (it is a caller-side
    /// projection of the hit stream, like `--count`).
    pub flags: GrepFlags,
    /// Walk gitignored files too (default: skip them).
    pub include_gitignored: bool,
    /// Walk hidden files/dirs too (default: skip them).
    pub include_hidden: bool,
    /// `--exclude-pattern` set: a path matching any pattern is not searched.
    pub exclude: Arc<ExcludeSet>,
    /// Binary-skip classifier (misc 135): a file it deems binary is skipped and
    /// recorded, never silently dropped. `None` disables the binary gate — the
    /// protocol-skeleton tests only; the production walk always supplies one.
    pub fs_manager: Option<Arc<FilesystemManager>>,
}

/// What one walk did.
///
/// How many ordered batches it emitted, which files it skipped instead of
/// searching (misc 135 — a skip is reported, never silent), and every regular
/// file it visited (the WS31 observation set).
#[derive(Debug, Default)]
pub struct WalkSummary {
    /// Number of [`HitBatch`]es handed to `on_batch` (0-based `seq`s, gap-free).
    pub batches: u64,
    /// Files skipped instead of searched, with their reasons. Folded into the
    /// wire-ready `GrepSkips` at the CLI cutover seam.
    pub skips: Vec<SkipRecord>,
    /// The observation **tail**: files visited after the last batch flushed
    /// (per-batch observations ride each [`HitBatch::observed`]) — the
    /// trailing files that held no hits, or the entire visited set when the
    /// walk emitted no batch at all. The daemon-stream sink ships it on the
    /// `End` terminator (suppressed for a zero-match walk — executor parity: a
    /// query with no matches never nudged) so the walk-level reap diff still
    /// covers every visited file.
    pub observed: Vec<(PathBuf, i64)>,
}

/// Walks `roots` for `pattern`, emitting each ordered [`HitBatch`] to `on_batch`.
///
/// The walk visits files in the `ignore` walker's order (path-sorted within a
/// root) and matches each line with the same `grep-*` engine the query path
/// uses — the matcher and searcher come from the executor's own constructors,
/// so case/word/fixed semantics and context/invert selection are identical by
/// construction. Context lines (`-A`/`-B`/`-C`) and inverted selections (`-v`)
/// become hits like any match line, carrying column `0` (no match column).
/// Hits accumulate into batches of at most [`HIT_BATCH_SIZE`]; a full batch is
/// handed to `on_batch` immediately (nothing is buffered into a whole-result
/// set), and a final short batch flushes at the end. `on_batch` receives
/// batches in strict `seq` order.
///
/// Every hit path is canonicalized at this seam ([`canonicalize_hit_path`]).
///
/// A named **file** root bypasses the gitignore/hidden gate and the positive
/// `-g`/`--type` filters — those govern recursive directory traversal, not a
/// path the caller named (misc 110, ripgrep parity).
///
/// Multi-root discipline (ws43-02, the executor's sort/dedup preserved): the
/// roots are walked in sorted, deduplicated order, and a file already searched
/// under an earlier root is never searched again — so overlapping roots
/// (`grep pat . src`) emit each hit exactly once, and the emission order is the
/// same global `(file, line)` order the query executor's sorted merge produced
/// (nested roots interleave naturally: the ancestor's own sorted walk covers
/// them in place). The one divergence from the buffered merge is a *named* root
/// the ancestor's walk filtered out (a gitignored named file): it emits at its
/// own root's sorted turn rather than interleaved — same hit set, later
/// position.
///
/// Returns a [`WalkSummary`]: the emitted batch count, the skip records, and
/// the visited-file observation set.
///
/// # Errors
///
/// Returns an error if the pattern is not a valid regex, a `-g`/`--type`
/// filter does not compile, or a root cannot be walked. A per-file read error
/// is skipped (logged at debug), never fatal — a walk degrades to fewer hits
/// from an unreadable file, never to no results.
pub fn walk<F>(
    pattern: &str,
    roots: &[PathBuf],
    options: &WalkOptions,
    mut on_batch: F,
) -> Result<WalkSummary>
where
    F: FnMut(HitBatch) -> Result<()>,
{
    let matcher = crate::bridge::build_matcher(pattern, &options.flags)?;
    let types = build_types(&options.flags)?;
    let mut batcher = Batcher::new(HIT_BATCH_SIZE, &mut on_batch);
    let mut skips: Vec<SkipRecord> = Vec::new();
    // One tracked-set consultation for the whole walk (misc 227): every root
    // shares it, so a multi-root grep over one repository reads that
    // repository's index at most once. See `crate::tracked` for the bound.
    let tracked = crate::tracked::TrackedHidden::new();
    // Canonical paths already searched — the cross-root dedup set. Same memory
    // order as the observation set (both are O(visited files), the executor's
    // own accumulation shape); the hit stream itself stays streaming.
    let mut dedup: HashSet<PathBuf> = HashSet::new();

    // Sorted, deduplicated roots: global emission order by construction.
    let mut ordered: Vec<&PathBuf> = roots.iter().collect();
    ordered.sort();
    ordered.dedup();

    for root in ordered {
        walk_root(
            root,
            &matcher,
            types.as_ref(),
            options,
            &tracked,
            &mut batcher,
            &mut skips,
            &mut dedup,
        )?;
    }

    let (emitted, observed_tail) = batcher.finish()?;
    Ok(WalkSummary {
        batches: emitted,
        skips,
        observed: observed_tail,
    })
}

/// Validates the walk's compilable inputs — the pattern, the `-t`/`--type`
/// filters, and the `-g`/`--glob` patterns — without walking anything.
///
/// The CLI's exit-2 usage gate (bug 105 contract, preserved through the
/// ws43-02 cutover): an uncompilable input is refused loudly *before* any
/// stream opens, so a walk that has started can no longer fail on user input.
/// The error spellings are exactly the walk's own (the same constructors run).
///
/// # Errors
///
/// Returns the same error [`walk`] would: an invalid regex pattern, `--type`
/// filter, or `--glob` pattern.
pub fn validate_inputs(pattern: &str, flags: &GrepFlags) -> Result<()> {
    crate::bridge::build_matcher(pattern, flags)?;
    build_types(flags)?;
    if !flags.globs.is_empty() {
        let mut ob = OverrideBuilder::new(Path::new("/"));
        for g in &flags.globs {
            ob.add(g)
                .map_err(|e| anyhow!("Invalid --glob pattern '{g}': {e}"))?;
        }
        ob.build()
            .map_err(|e| anyhow!("Invalid --glob filter: {e}"))?;
    }
    Ok(())
}

/// Resolves the `-t`/`--type` filters against ripgrep's built-in type
/// definitions — root-independent, so built once per walk.
fn build_types(flags: &GrepFlags) -> Result<Option<Types>> {
    if flags.types.is_empty() {
        return Ok(None);
    }
    let mut tb = TypesBuilder::new();
    tb.add_defaults();
    for t in &flags.types {
        tb.select(t);
    }
    Ok(Some(
        tb.build()
            .map_err(|e| anyhow!("Invalid --type filter: {e}"))?,
    ))
}

/// Walks one root, feeding every match into `batcher` in walk order, every
/// binary skip into `skips`, and every visited regular file into the
/// batcher's pending observation set (the WS31 observations, shipped with the
/// next batch). `dedup` is the cross-root canonical-path dedup set — a file
/// already searched under an earlier root is skipped. `tracked` is the walk-wide
/// tracked-set consultation behind the hidden posture (misc 227).
#[allow(
    clippy::too_many_arguments,
    reason = "one root's full walk context; the tracked-set consultation is walk-wide by design"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one linear pass: observation, filter gates, binary skip, then the search"
)]
fn walk_root<F>(
    root: &Path,
    matcher: &grep_regex::RegexMatcher,
    types: Option<&Types>,
    options: &WalkOptions,
    tracked: &Arc<crate::tracked::TrackedHidden>,
    batcher: &mut Batcher<'_, F>,
    skips: &mut Vec<SkipRecord>,
    dedup: &mut HashSet<PathBuf>,
) -> Result<()>
where
    F: FnMut(HitBatch) -> Result<()>,
{
    use ignore::WalkBuilder;

    // A named file bypasses the gitignore/hidden gate and the positive file
    // filters — those govern recursive directory traversal, not a path the
    // caller named (misc 110, ripgrep parity). Mirrors the query engine's
    // `root_is_file` bypass.
    let root_is_file = root.is_file();
    let skip_gitignored = !options.include_gitignored && !root_is_file;
    let skip_hidden = !options.include_hidden && !root_is_file;

    let mut builder = WalkBuilder::new(root);
    builder
        .git_ignore(skip_gitignored)
        .sort_by_file_path(std::cmp::Ord::cmp);
    // "Tracked beats hidden" (misc 227): a dot-leading path git tracks joins the
    // default walk; untracked hidden stays skipped; `--include-hidden`
    // (`skip_hidden == false`) is unchanged.
    crate::tracked::apply_hidden_posture(&mut builder, root, skip_hidden, tracked);
    if !root_is_file {
        if !options.flags.globs.is_empty() {
            let mut ob = OverrideBuilder::new(root);
            for g in &options.flags.globs {
                ob.add(g)
                    .map_err(|e| anyhow!("Invalid --glob pattern '{g}': {e}"))?;
            }
            builder.overrides(
                ob.build()
                    .map_err(|e| anyhow!("Invalid --glob filter: {e}"))?,
            );
        }
        if let Some(types) = types {
            builder.types(types.clone());
        }
    }
    let walker = builder.build();

    let mut searcher = crate::bridge::build_searcher(&options.flags);

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

        // Record this file's mtime for the WS31 changed-set observation set —
        // every visited file, before the query-level exclude and binary skips,
        // exactly as the query executor recorded it. The stat is retried (a
        // fresh stat can race an atomic rename even when `d_type` already
        // proved the entry a file) and reused by the binary check below. An
        // enumerated present file whose stat still misses is recorded with the
        // `OBSERVED_STAT_MISS_MTIME` sentinel, NOT omitted: omission would let
        // a full walk false-reap it as `Deleted` (WS31-review H1).
        let metadata = stat_with_retry(path);
        let stat_mtime = metadata
            .as_ref()
            .map_or(OBSERVED_STAT_MISS_MTIME, mtime_nanos);
        batcher.observe(path.to_path_buf(), stat_mtime);

        // Query-level exclusion (`--exclude-pattern`): matched paths are not
        // searched, exactly as the query engine gates them.
        if options.exclude.is_match(path, root) {
            continue;
        }

        // Skip a file the classifier treats as binary (a NUL byte before any
        // text BOM) — recorded with its reason and whether it was explicitly
        // named, never silently dropped (misc 135). A stat that misses (a
        // live-race rename) searches the file rather than dropping it,
        // mirroring the query engine.
        if let Some(fs) = &options.fs_manager
            && let Some(md) = &metadata
            && let Some(reason) = fs.binary_skip_reason(path, md)
        {
            skips.push(SkipRecord {
                path: path.to_path_buf(),
                reason,
                named: root_is_file,
            });
            continue;
        }

        // Canonicalize at the ingestion seam — the hit carries a canonical path.
        let canonical = canonicalize_hit_path(path);

        // Cross-root dedup: overlapping roots (`grep pat . src`) visit a file
        // more than once; only its first (sorted-order) visit searches it, so
        // each hit is emitted exactly once (the executor's map-keyed dedup,
        // preserved).
        if !dedup.insert(canonical.clone()) {
            continue;
        }

        let mut sink = HitSink {
            matcher,
            path: &canonical,
            invert: options.flags.invert,
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
/// Observations accumulate alongside and drain into each flushed batch, so
/// they reach the daemon's nudge in walk order, ahead of the batch's anchors.
struct Batcher<'a, F> {
    cap: usize,
    seq: u64,
    current: Vec<Hit>,
    /// Observations recorded since the last flush — drained into the next
    /// batch; whatever remains at `finish` is the walk's observation tail.
    pending_observed: Vec<(PathBuf, i64)>,
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
            pending_observed: Vec::new(),
            on_batch,
        }
    }

    /// Records one visited file's observation (shipped with the next batch).
    fn observe(&mut self, path: PathBuf, mtime: i64) {
        self.pending_observed.push((path, mtime));
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
            observed: std::mem::take(&mut self.pending_observed),
        };
        self.seq += 1;
        (self.on_batch)(batch)
    }

    /// Flushes any partial final batch and returns the total batch count plus
    /// the observation tail (files visited after the last flushed batch).
    fn finish(mut self) -> Result<(u64, Vec<(PathBuf, i64)>)> {
        self.flush()?;
        Ok((self.seq, self.pending_observed))
    }
}

/// A `grep-searcher` sink that turns each selected line — a match, an inverted
/// selection, or a context line — into a [`Hit`] and pushes it onto the
/// batcher. A batcher-callback error is captured in `error` (the searcher
/// `Sink` trait's error type is I/O-shaped, so a caller-callback error is
/// stashed and re-surfaced by the walk).
struct HitSink<'a, 'b, F> {
    matcher: &'a grep_regex::RegexMatcher,
    path: &'a Path,
    /// `-v`: the selected lines do not match, so they carry no match column.
    invert: bool,
    batcher: &'a mut Batcher<'b, F>,
    error: Option<anyhow::Error>,
}

impl<F> HitSink<'_, '_, F>
where
    F: FnMut(HitBatch) -> Result<()>,
{
    /// Records one selected line as a hit with the given match column
    /// (`0` when the line carries no match — context or inverted selection).
    fn record(&mut self, line: u64, bytes: &[u8], column: u32) -> bool {
        let text = String::from_utf8_lossy(bytes)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        let hit = Hit {
            path: self.path.to_path_buf(),
            line: u32::try_from(line).unwrap_or(0),
            column,
            text,
        };
        if let Err(e) = self.batcher.push(hit) {
            self.error = Some(e);
            return false;
        }
        true
    }
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

        // 1-based column of the first match on the line (ripgrep display
        // convention). Under `-v` the selected line does NOT match, so there is
        // no column to find — record 0, like a context line.
        let column = if self.invert {
            0
        } else {
            self.matcher
                .find_at(mat.bytes(), 0)
                .ok()
                .flatten()
                .map_or(0, |m| {
                    u32::try_from(m.start()).unwrap_or(0).saturating_add(1)
                })
        };

        Ok(self.record(line, mat.bytes(), column))
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> std::result::Result<bool, std::io::Error> {
        // Context lines (`-A`/`-B`/`-C`) become hits like match lines — each is
        // a self-contained result line anchored at its own coordinates. No
        // match column.
        if self.error.is_some() {
            return Ok(false);
        }
        let line = ctx.line_number().unwrap_or(0);
        Ok(self.record(line, ctx.bytes(), 0))
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

    /// Collects every hit of a walk with the given options.
    fn collect_hits(pattern: &str, roots: &[PathBuf], options: &WalkOptions) -> Vec<Hit> {
        collect_batches(pattern, roots, options)
            .expect("walk")
            .into_iter()
            .flat_map(|b| b.hits)
            .collect()
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

    // ─── the full flag surface (ws43-02) ────────────────────────────────

    #[test]
    fn context_lines_become_hits_with_no_match_column() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "f.txt", "before\nthe needle line\nafter\n");

        let options = WalkOptions {
            flags: GrepFlags {
                before_context: 1,
                after_context: 1,
                ..GrepFlags::default()
            },
            ..WalkOptions::default()
        };
        let hits = collect_hits("needle", &[tmp.path().to_path_buf()], &options);
        let lines: Vec<(u32, u32)> = hits.iter().map(|h| (h.line, h.column)).collect();
        assert_eq!(
            lines,
            vec![(1, 0), (2, 5), (3, 0)],
            "context lines ride as hits (column 0), the match keeps its column"
        );
        assert_eq!(hits[0].text, "before");
        assert_eq!(hits[2].text, "after");
    }

    #[test]
    fn invert_selects_non_matching_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "f.txt", "keep one\ndrop needle\nkeep two\n");

        let options = WalkOptions {
            flags: GrepFlags {
                invert: true,
                ..GrepFlags::default()
            },
            ..WalkOptions::default()
        };
        let hits = collect_hits("needle", &[tmp.path().to_path_buf()], &options);
        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["keep one", "keep two"]);
        assert!(
            hits.iter().all(|h| h.column == 0),
            "inverted selections carry no match column"
        );
    }

    #[test]
    fn case_flags_ride_the_shared_matcher() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "f.txt", "Needle up\nneedle down\n");
        let roots = vec![tmp.path().to_path_buf()];

        // Smart-case default: an uppercase-bearing pattern is sensitive.
        let sensitive = collect_hits("Needle", &roots, &WalkOptions::default());
        assert_eq!(sensitive.len(), 1, "smart-case keeps `Needle` off line 2");

        // `-i` forces insensitive.
        let mut forced = WalkOptions::default();
        forced.flags.ignore_case = true;
        assert_eq!(collect_hits("Needle", &roots, &forced).len(), 2);

        // `-s` forces sensitive even for a lowercase pattern.
        let mut strict = WalkOptions::default();
        strict.flags.case_sensitive = true;
        assert_eq!(collect_hits("needle", &roots, &strict).len(), 1);
    }

    #[test]
    fn glob_filter_restricts_directory_walks_but_not_named_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "a.rs", "let needle = 1;\n");
        write_file(tmp.path(), "b.txt", "needle in text\n");

        let mut options = WalkOptions::default();
        options.flags.globs = vec!["*.rs".to_string()];

        let hits = collect_hits("needle", &[tmp.path().to_path_buf()], &options);
        assert_eq!(hits.len(), 1, "-g '*.rs' filters the .txt file");
        assert!(hits[0].path.ends_with("a.rs"));

        // A named file bypasses the positive filter — naming it is a direct
        // request (misc 110).
        let named = collect_hits("needle", &[tmp.path().join("b.txt")], &options);
        assert_eq!(named.len(), 1, "a named file is searched despite -g");
    }

    #[test]
    fn type_filter_restricts_to_matching_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "a.rs", "let needle = 1;\n");
        write_file(tmp.path(), "b.md", "needle in prose\n");

        let mut options = WalkOptions::default();
        options.flags.types = vec!["rust".to_string()];

        let hits = collect_hits("needle", &[tmp.path().to_path_buf()], &options);
        assert_eq!(hits.len(), 1, "--type rust keeps only the .rs file");
        assert!(hits[0].path.ends_with("a.rs"));
    }

    #[test]
    fn exclude_set_gates_the_walk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "keep.txt", "needle kept\n");
        write_file(tmp.path(), "drop.txt", "needle dropped\n");

        let exclude =
            ExcludeSet::compile(&[tmp.path().join("drop.txt").to_string_lossy().into_owned()])
                .expect("compile exclude");
        let options = WalkOptions {
            exclude: Arc::new(exclude),
            ..WalkOptions::default()
        };
        let hits = collect_hits("needle", &[tmp.path().to_path_buf()], &options);
        assert_eq!(hits.len(), 1, "the excluded file is not searched");
        assert!(hits[0].path.ends_with("keep.txt"));
    }

    #[test]
    fn binary_file_is_skipped_and_recorded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "text.txt", "needle text\n");
        // A NUL byte before any text BOM classifies as binary.
        std::fs::write(tmp.path().join("blob.bin"), b"nee\x00dle binary\n").expect("write binary");

        let fs = FilesystemManager::new();
        fs.set_roots(vec![tmp.path().to_path_buf()]);
        let options = WalkOptions {
            fs_manager: Some(Arc::new(fs)),
            ..WalkOptions::default()
        };

        let mut hits: Vec<Hit> = Vec::new();
        let summary = walk("needle", &[tmp.path().to_path_buf()], &options, |batch| {
            hits.extend(batch.hits);
            Ok(())
        })
        .expect("walk");

        assert_eq!(hits.len(), 1, "only the text file is searched");
        assert!(hits[0].path.ends_with("text.txt"));
        assert_eq!(summary.skips.len(), 1, "the binary skip is recorded");
        assert!(summary.skips[0].path.ends_with("blob.bin"));
        assert!(!summary.skips[0].named, "a walked skip is not named");
    }

    #[test]
    fn streaming_callback_fires_before_the_walk_completes() {
        // The no-unbounded-buffering pin: batches are handed to the callback
        // DURING the walk, not collected and delivered at the end. Proof by
        // interference: the first batch's callback deletes a file the
        // path-sorted walk has not reached yet — its hits must then be absent.
        // A walk that buffered the whole result set before invoking callbacks
        // would have already read the file and emitted its hits.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let mut body = String::new();
        for _ in 0..HIT_BATCH_SIZE {
            body.push_str("needle early\n");
        }
        write_file(root, "aaa.txt", &body);
        write_file(root, "zzz.txt", "needle late\n");
        let zzz = root.join("zzz.txt");

        let mut hits: Vec<Hit> = Vec::new();
        let mut deleted = false;
        walk(
            "needle",
            &[root.to_path_buf()],
            &WalkOptions::default(),
            |batch| {
                if !deleted {
                    std::fs::remove_file(&zzz).expect("delete zzz mid-walk");
                    deleted = true;
                }
                hits.extend(batch.hits);
                Ok(())
            },
        )
        .expect("walk");

        assert!(deleted, "the first batch fired during the walk");
        assert_eq!(
            hits.len(),
            HIT_BATCH_SIZE,
            "only aaa.txt's hits are emitted"
        );
        assert!(
            hits.iter().all(|h| !h.path.ends_with("zzz.txt")),
            "the file deleted by the first batch's callback was never searched — \
             emission streams ahead of the walk, nothing buffers the result set"
        );
    }

    /// Initializes a git repository so `.gitignore` rules apply (gitignore is
    /// repo-scoped, matching ripgrep).
    fn git_init(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");
    }

    #[test]
    fn named_gitignored_file_is_searched_but_dir_walk_still_gates() {
        // misc 110, migrated from the retired executor's pins: a gitignored
        // file NAMED as a root is searched unconditionally (naming it is a
        // direct request), while the same file reached by walking a directory
        // root stays gated behind --include-gitignored.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").expect("write gitignore");
        write_file(root, "kept.txt", "needle kept\n");
        write_file(root, "ignored.txt", "needle ignored\n");

        // Directory walk: the gitignored file is gated.
        let walked = collect_hits("needle", &[root.to_path_buf()], &WalkOptions::default());
        assert!(
            walked.iter().all(|h| !h.path.ends_with("ignored.txt")),
            "a directory walk gates gitignored contents"
        );
        assert!(
            walked.iter().any(|h| h.path.ends_with("kept.txt")),
            "the non-ignored file is searched"
        );

        // Named file root: the gate does not apply.
        let named = collect_hits(
            "needle",
            &[root.join("ignored.txt")],
            &WalkOptions::default(),
        );
        assert_eq!(named.len(), 1, "a named gitignored file is searched");

        // The escape hatch lifts the directory-walk gate.
        let options = WalkOptions {
            include_gitignored: true,
            ..WalkOptions::default()
        };
        let with_ignored = collect_hits("needle", &[root.to_path_buf()], &options);
        assert!(
            with_ignored.iter().any(|h| h.path.ends_with("ignored.txt")),
            "--include-gitignored surfaces the gated file"
        );
    }

    #[test]
    fn empty_pattern_matches_every_line() {
        // bug 83 (ripgrep parity), migrated from the retired executor's pins:
        // the empty pattern matches every line — a `--count` projection over
        // this walk is the file's line count, never a silent zero.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(
            tmp.path(),
            "fixture.txt",
            "alpha\nbeta\ngamma\ndelta\nepsilon\n",
        );
        let hits = collect_hits("", &[tmp.path().to_path_buf()], &WalkOptions::default());
        assert_eq!(hits.len(), 5, "empty pattern selects every line");
    }

    #[test]
    fn overlapping_roots_dedup_and_keep_global_order() {
        // ws43-02 multi-root discipline: overlapping roots (`grep pat . sub`)
        // emit each hit exactly once, and sorted-root walking preserves the
        // global (file, line) order the executor's merged sort produced —
        // regardless of the argument order.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_file(root, "a.txt", "needle a\n");
        write_file(root, "sub/b.txt", "needle b\n");
        write_file(root, "z.txt", "needle z\n");

        let roots = vec![
            root.join("sub"),
            root.to_path_buf(),
            root.join("sub"), // exact duplicate root
        ];
        let hits = collect_hits("needle", &roots, &WalkOptions::default());
        let names: Vec<String> = hits
            .iter()
            .map(|h| {
                h.path
                    .strip_prefix(root.canonicalize().expect("canonical root"))
                    .expect("hit under root")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "a.txt".to_string(),
                "sub/b.txt".to_string(),
                "z.txt".to_string()
            ],
            "each hit once, in global path order"
        );
    }

    #[test]
    fn walk_records_visited_files_as_observations() {
        // ws43-02 reap parity: every visited regular file lands in the
        // observation stream with a real mtime — matched or not — batched
        // observations riding each flush and the remainder in the summary
        // tail, so the daemon's nudges see the same set the executor observed.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(tmp.path(), "hit.txt", "needle\n");
        write_file(tmp.path(), "miss.txt", "nothing here\n");

        let mut observed: Vec<(PathBuf, i64)> = Vec::new();
        let summary = walk(
            "needle",
            &[tmp.path().to_path_buf()],
            &WalkOptions::default(),
            |batch| {
                observed.extend(batch.observed);
                Ok(())
            },
        )
        .expect("walk");
        observed.extend(summary.observed);
        assert_eq!(
            observed.len(),
            2,
            "both visited files are observed: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .any(|(p, m)| p.ends_with("miss.txt") && *m != OBSERVED_STAT_MISS_MTIME),
            "an unmatched file is still observed with its mtime"
        );
        // hit.txt matched, so the batch that carries it also carries every
        // observation recorded up to its flush — the order the cold-snapshot
        // classification relies on.
        assert!(
            observed.iter().any(|(p, _)| p.ends_with("hit.txt")),
            "the matched file is observed too"
        );
    }

    #[test]
    fn named_binary_file_records_a_named_skip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let blob = tmp.path().join("blob.bin");
        std::fs::write(&blob, b"\x00binary\n").expect("write binary");

        let fs = FilesystemManager::new();
        fs.set_roots(vec![tmp.path().to_path_buf()]);
        let options = WalkOptions {
            fs_manager: Some(Arc::new(fs)),
            ..WalkOptions::default()
        };

        let summary =
            walk("binary", std::slice::from_ref(&blob), &options, |_| Ok(())).expect("walk");
        assert_eq!(summary.batches, 0, "no batch from a fully-skipped walk");
        assert_eq!(summary.skips.len(), 1);
        assert!(
            summary.skips[0].named,
            "a named path's skip is per-file (misc 135)"
        );
    }
}
