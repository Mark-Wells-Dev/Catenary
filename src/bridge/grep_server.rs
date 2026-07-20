// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grep's shared machinery: flags, skips, matcher/searcher construction, the
//! LSP enrichment core, and stdin (stream) mode.
//!
//! ## ws43 status (query streaming)
//!
//! The query-streaming rework (ws43) replaced the daemon-side query executors
//! with CLI-owned walks (grep: [`crate::hitstream::engine`]; glob: the plan
//! build in [`super::file_tools`]) plus one daemon-side annotator
//! ([`super::hitstream_enricher::HitstreamEnricher`]). The grep executor
//! (`GrepServer::execute`, the ripgrep parallel walk, the hunk shaping/spool,
//! and the `tool/grep` dispatch arm) retired with the ws43-02 CLI cutover;
//! the glob executor and its `tool/glob` arm retired with ws43-03, and
//! `GrepServer` (by then only the annotator-builder holder) retired with them
//! — the annotator is built by
//! [`Session::hitstream_enricher`](super::session::Session::hitstream_enricher).
//! What remains here is everything the walks always shared — and the
//! annotator still runs:
//!
//! - [`GrepFlags`] — the one flag surface the CLI, the walk, and stdin mode
//!   carry;
//! - [`GrepSkips`]/[`SkipRecord`] — the misc-135 skip reporting, folded
//!   CLI-side from the walk's records;
//! - [`build_matcher`]/[`build_searcher`] — the matcher/searcher constructors
//!   the hitstream engine walks with;
//! - the shared enrichment core ([`anchor_context`],
//!   [`nudge_observed_files`]) behind the annotator;
//! - stdin (stream) mode ([`grep_stream`]), which never involved the daemon.

use anyhow::{Result, anyhow};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::filesystem_manager::{BinarySkip, FilesystemManager};
use super::handler::display_path;
use crate::config::DispatchMethod;
use crate::lsp::server::LspServer;
use crate::lsp::{LspClientManager, WalkBreadth};
use crate::symbol_index::{ScopeFilter, Symbol, SymbolIndex};

/// Ripgrep-parity flags shared across the grep surfaces (`catenary grep`'s CLI
/// and the hitstream engine's [`WalkOptions`](crate::hitstream::WalkOptions)).
///
/// These make `catenary grep` a strict ripgrep superset on the *input* surface
/// (it is already one on results): the matcher modifiers (`-i`/`-s`/`-w`/`-F`),
/// the line-selection modifiers (`-v`, context `-A`/`-B`/`-C`), the positive file
/// filters (`-g`/`--type`), and the files-with-matches view (`-l`). They are
/// carried identically into stdin mode (which only drops enrichment, never
/// capability) and into file mode. Every field is `#[serde(default)]` so an older
/// or minimal wire payload round-trips, and skipped when empty so a flagless query
/// serializes exactly as before.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "orthogonal ripgrep flags, 1:1 with the clap-parsed grep surface"
)]
pub struct GrepFlags {
    /// `-i`/`--ignore-case`: force case-insensitive matching (overrides smart-case).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_case: bool,
    /// `-s`/`--case-sensitive`: force case-sensitive matching (overrides smart-case).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub case_sensitive: bool,
    /// `-w`/`--word-regexp`: only match on word boundaries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub word: bool,
    /// `-F`/`--fixed-strings`: treat the pattern as a literal string, not a regex.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fixed_strings: bool,
    /// `-v`/`--invert-match`: select the lines that do *not* match.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    /// `-l`/`--files-with-matches`: print only the paths of files with a match.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub files_with_matches: bool,
    /// `-B`/`--before-context` (also set by `-C`): lines of context before a match.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub before_context: usize,
    /// `-A`/`--after-context` (also set by `-C`): lines of context after a match.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub after_context: usize,
    /// `-g`/`--glob`: positive include globs (ripgrep semantics — a leading `!`
    /// negates). When any non-negated glob is present, only matching files are
    /// searched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    /// `-t`/`--type`: ripgrep file-type filters (e.g. `rust`, `md`), resolved
    /// against ripgrep's built-in type definitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
}

/// `serde` `skip_serializing_if` predicate for `usize` fields that default to 0.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires a &T predicate"
)]
const fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The `#scope` graph coordinate appended to a grep line — a containment trail,
/// not a resolvable path. The `#` scheme carries degradation natively (bug 48).
///
/// `pub(super)` since ws43-02: the hitstream annotator
/// ([`super::hitstream_enricher`]) maps this tri-state onto the wire
/// [`AnnotatedHit`](crate::hitstream::frame::AnnotatedHit), whose
/// `render_grep_line` is the (pinned) reproduction of the retired executor's
/// line rendering.
pub(super) enum Anchor {
    /// Enriched, inside a scope → `#<trail>`: the `/`-joined chain of enclosing
    /// *named* symbols (slugified), outermost→hit. Already joined.
    Scope(String),
    /// Enriched, genuinely top-level (no enclosing symbol) → no `#` at all: a
    /// pure ripgrep line. Honest — there is no graph coordinate to report.
    TopLevel,
    /// Could not enrich (no server, language unserved, request failed) → `#?`,
    /// a distinct marker so degradation is never misread as top-level and it
    /// survives a pipe.
    Unknown,
}

/// Files in the search scope that were skipped instead of searched (misc 135,
/// bug 62).
///
/// Carried alongside every grep result so a skip is reported, never silent.
/// Empty for the overwhelmingly common all-searched query, so a normal result
/// renders byte-identically to before (no skip lines, no count suffix).
///
/// Explicitly-**named** files (a positional path arg, or a glob that expanded to
/// the file) are reported per-file in `named` — a named path is a direct request
/// and must never silently vanish. Directory-walk skips of **unnamed** files are
/// aggregated by reason in `walked` so a binary-heavy tree cannot flood output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrepSkips {
    /// Explicitly-named skipped files: `(cwd-relative path, reason label)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named: Vec<(String, String)>,
    /// Directory-walk skips of unnamed files, aggregated by reason:
    /// `(reason label, count)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub walked: Vec<(String, usize)>,
}

impl GrepSkips {
    /// True when nothing was skipped — the common case, rendered byte-identically
    /// to a pre-misc-135 result.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.named.is_empty() && self.walked.is_empty()
    }

    /// Total number of skipped files (per-file named + aggregated walked).
    #[must_use]
    pub fn total(&self) -> usize {
        self.named.len() + self.walked.iter().map(|(_, n)| *n).sum::<usize>()
    }

    /// The per-file and aggregate skip lines appended to the default (and `-l`)
    /// grep output. A named file gets `skipped (<reason>): <path>`; walked files
    /// collapse to `<n> file(s) skipped (<reason>)`.
    #[must_use]
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.named.len() + self.walked.len());
        for (path, reason) in &self.named {
            lines.push(format!("skipped ({reason}): {path}"));
        }
        for (reason, count) in &self.walked {
            let noun = if *count == 1 { "file" } else { "files" };
            lines.push(format!("{count} {noun} skipped ({reason})"));
        }
        lines
    }

    /// The `--count` suffix, e.g. ` (1 skipped: binary)`, or `None` when nothing
    /// was skipped (the count then renders exactly as before). A single distinct
    /// reason is named plainly; multiple reasons list a per-reason breakdown so
    /// `skipped` is never conflated with no-match. Content classification now
    /// leaves `binary` as the only reason (misc 140), but the breakdown stays
    /// generic over the reason label.
    #[must_use]
    pub fn count_suffix(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let total = self.total();
        let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, reason) in &self.named {
            *by_reason.entry(reason.as_str()).or_default() += 1;
        }
        for (reason, count) in &self.walked {
            *by_reason.entry(reason.as_str()).or_default() += *count;
        }
        let breakdown = if by_reason.len() == 1 {
            by_reason
                .keys()
                .next()
                .copied()
                .unwrap_or_default()
                .to_string()
        } else {
            by_reason
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Some(format!(" ({total} skipped: {breakdown})"))
    }

    /// Folds the raw [`SkipRecord`]s from a walk into the wire-ready
    /// named/walked split, resolving each path to its display form.
    /// Every path is counted once — a path both named and walked is reported as
    /// named (the stronger, per-file signal).
    ///
    /// `pub` since ws43-02: the CLI folds the hitstream walk's skip records
    /// through this same function, so a skip renders identically to the
    /// retired executor's rendering.
    #[must_use]
    pub fn from_records(
        records: &[SkipRecord],
        fs_manager: &FilesystemManager,
        cwd: Option<&Path>,
    ) -> Self {
        let mut named: Vec<(String, String)> = Vec::new();
        let mut walked_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        // Named first, so a path named *and* walked is reported per-file.
        for rec in records.iter().filter(|r| r.named) {
            if seen.insert(rec.path.clone()) {
                named.push((
                    rel_path(&rec.path, fs_manager, cwd),
                    rec.reason.label().to_string(),
                ));
            }
        }
        for rec in records.iter().filter(|r| !r.named) {
            if seen.insert(rec.path.clone()) {
                *walked_counts
                    .entry(rec.reason.label().to_string())
                    .or_default() += 1;
            }
        }
        named.sort();
        Self {
            named,
            walked: walked_counts.into_iter().collect(),
        }
    }
}

// ─── Shared enrichment core (the ws43 hitstream annotator's) ─────────────
//
// The pieces below are the single implementation of grep's LSP enrichment.
// They were shared by the retired query executor and the streamed-engine
// annotator (`super::hitstream_enricher`) while the two coexisted (ws43-02:
// move logic, don't duplicate it); since the cutover the annotator is the sole
// caller.

/// Per-file enrichment context for a set of matched paths: the `documentSymbol`
/// outlines (the sole source of the `#scope` anchor) plus the set of files that
/// could not be enriched at all.
///
/// Built once per hit-batch by [`anchor_context`]; consumed per hit via
/// [`Self::anchor_for`].
pub(super) struct AnchorContext {
    /// `documentSymbol` outlines per matched file.
    file_symbols: HashMap<PathBuf, Vec<Symbol>>,
    /// Files with no `documentSymbol` coverage (no live capable server): their
    /// hits carry the `#?` could-not-enrich marker.
    uncovered: HashSet<PathBuf>,
}

impl AnchorContext {
    /// Builds the context from file-grade parts — the sweep tier's
    /// construction (brackets 04).
    ///
    /// The sweep path derives `file_symbols` per file from a rootless
    /// single-file singleton's `documentSymbol` answer (never a root
    /// instance), and `uncovered` is decided capability-shaped: a file whose
    /// language has no capable singleton, or whose bracket degraded, renders
    /// raw (`#?` / `no outline`). The consuming projections
    /// ([`Self::anchor_for`], [`Self::symbols_for`]) are shared with the dig
    /// tier, so the two tiers' hit shapes cannot drift.
    pub(super) const fn from_file_grade(
        file_symbols: HashMap<PathBuf, Vec<Symbol>>,
        uncovered: HashSet<PathBuf>,
    ) -> Self {
        Self {
            file_symbols,
            uncovered,
        }
    }

    /// Whether `file` could not be enriched at all (no `documentSymbol`
    /// coverage — the `#?` / `no outline` degrade state).
    pub(super) fn is_uncovered(&self, file: &Path) -> bool {
        self.uncovered.contains(file)
    }

    /// The file's `documentSymbol` outline (every depth, ascending declaration
    /// line — the index's stored order), or `None` when the file has no
    /// indexed symbols. The ws43-03 outline annotator reads whole files from
    /// the same context the grep anchors are derived from.
    pub(super) fn symbols_for(&self, file: &Path) -> Option<&[Symbol]> {
        self.file_symbols.get(file).map(Vec::as_slice)
    }

    /// The `#scope` anchor for a hit in `file` at 0-based `line_0`: `#?` when
    /// the file could not be enriched, no anchor when the hit is genuinely
    /// top-level, and the slugified containment trail otherwise.
    pub(super) fn anchor_for(&self, file: &Path, line_0: u32) -> Anchor {
        if self.uncovered.contains(file) {
            return Anchor::Unknown;
        }
        let trail = self
            .file_symbols
            .get(file)
            .map_or_else(Vec::new, |s| scope_trail(s, line_0));
        if trail.is_empty() {
            Anchor::TopLevel
        } else {
            Anchor::Scope(trail.join("/"))
        }
    }
}

/// Builds the [`AnchorContext`] for `paths`.
///
/// Populates (or refreshes) the symbol index for the matched files
/// ([`super::ensure_symbols`]), reads every symbol per file in one index query,
/// and classifies per-file coverage for the `#?` degradation marker. A file
/// with indexed symbols is covered. For a symbol-less file the two causes are
/// indistinguishable from the index alone — "enriched, but genuinely empty"
/// (no `#`) versus "could not enrich" (`#?`) — so the live server registry
/// decides: a `documentSymbol`-capable, alive server for the file means
/// coverage (`get_servers` already filters dead servers, disabled methods, and
/// file-pattern mismatches). No round-trip. The per-hit nav suite (references /
/// call hierarchy / implementation / type hierarchy) does not fire: this is
/// goto-tier and essentially free (`O(files)`, no per-hit round-trip).
pub(super) async fn anchor_context(
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    client_manager: &LspClientManager,
    fs_manager: &FilesystemManager,
    paths: &[PathBuf],
    parent_id: Option<&str>,
) -> AnchorContext {
    super::ensure_symbols(symbol_index, client_manager, fs_manager, paths, parent_id).await;

    let path_refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let file_symbols: HashMap<PathBuf, Vec<Symbol>> =
        symbol_index.map_or_else(HashMap::new, |index_mutex| {
            let index = index_mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            index
                .query_scoped(&path_refs, &ScopeFilter::AnyDepth, "*", None, false)
                .unwrap_or_default()
        });

    let mut uncovered: HashSet<PathBuf> = HashSet::new();
    for path in paths {
        if file_symbols.contains_key(path) {
            continue;
        }
        let servers = client_manager
            .get_servers(
                path,
                LspServer::supports_document_symbols,
                Some(DispatchMethod::DocumentSymbol),
            )
            .await;
        if servers.is_empty() {
            uncovered.insert(path.clone());
        }
    }

    AnchorContext {
        file_symbols,
        uncovered,
    }
}

/// Routes the WS31 changed-set nudge (Consumer A) for a set of observed files
/// under the walk-breadth gate (ticket 04).
///
/// `observed_files` are `(absolute path, mtime-nanos)` observations — every
/// file the CLI walk visited (the walk-level `observe_walk` nudge, shipped on
/// the `End` terminator) or the canonical hit paths of one batch, freshly
/// statted (the annotator's per-batch nudge). They are grouped by registered
/// root (root-relative), diffed against the per-root baseline, and the delta
/// routed per server. A root with no covering server is `WalkBreadth::None` —
/// the `(no LSP)` case — and is skipped entirely (no diff, no nudge).
///
/// Reaping is gated per-root by whether the walk actually spanned the whole
/// registered root (WS31-review C1): `reap_scopes`, when `Some`, carries the
/// canonicalized scopes a *pathless* walk covered, and a root reaps only when
/// one of them is an ancestor-or-equal of it. `None` — a path-scoped query, or
/// the annotator's per-batch nudge, whose hit set never proves absence — is
/// add/update only, exactly like a scoped `glob`.
pub(super) async fn nudge_observed_files(
    client_manager: &LspClientManager,
    fs_manager: &FilesystemManager,
    observed_files: &[(PathBuf, i64)],
    reap_scopes: Option<&[PathBuf]>,
) {
    let mut by_root: HashMap<PathBuf, Vec<(PathBuf, i64)>> = HashMap::new();
    for (abs, mtime) in observed_files {
        if let Some(root) = fs_manager.resolve_root(abs)
            && let Ok(rel) = abs.strip_prefix(&root)
        {
            by_root
                .entry(root)
                .or_default()
                .push((rel.to_path_buf(), *mtime));
        }
    }
    let no_exclude: HashSet<PathBuf> = HashSet::new();
    for (root, observed) in &by_root {
        let breadth = if client_manager.has_covering_watchers(root).await {
            WalkBreadth::Full
        } else {
            WalkBreadth::None
        };
        if !breadth.runs_engine() {
            continue;
        }
        // Only reap when the walk truly covered the whole root: a pathless
        // walk whose scope is an ancestor-or-equal of this registered root. A
        // subtree walk cannot assert that an unvisited baseline entry is gone.
        let covered_whole_root =
            reap_scopes.is_some_and(|scopes| scopes.iter().any(|scope| root.starts_with(scope)));
        let reap = breadth.reaps() && covered_whole_root;
        client_manager
            .nudge_changed_set(root, observed, &no_exclude, reap)
            .await;
    }
}

// ─── Matcher / searcher construction ───────────────────────────────────

/// Builds the regex matcher for a grep query from the ripgrep-parity flags.
///
/// Case handling reconciles to **smart-case** (the residual design decision):
/// with neither `-i` nor `-s`, the matcher is case-insensitive *unless* the
/// pattern carries an uppercase letter; `-i` (`ignore_case`) forces insensitive
/// and `-s` (`case_sensitive`) forces sensitive. `-F` (`fixed_strings`) escapes
/// the pattern to a literal first (uppercase letters survive escaping, so
/// smart-case still sees them), and `-w` (`word`) wraps it in word boundaries.
///
/// # Errors
///
/// Returns an error if the (possibly escaped) pattern is not a valid regex.
///
/// Crate-visible since ws43-02 (re-exported `pub(crate)` from `bridge`): the
/// hitstream engine ([`crate::hitstream::engine`]) builds its CLI-side walk
/// matcher through this same constructor, so the two walks' matching semantics
/// cannot drift.
pub fn build_matcher(pattern: &str, flags: &GrepFlags) -> Result<RegexMatcher> {
    let effective = if flags.fixed_strings {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let mut builder = RegexMatcherBuilder::new();
    if flags.ignore_case {
        builder.case_insensitive(true);
    } else if flags.case_sensitive {
        builder.case_insensitive(false).case_smart(false);
    } else {
        builder.case_smart(true);
    }
    builder.word(flags.word);
    builder
        .build(&effective)
        .map_err(|e| anyhow!("Invalid regex pattern: {e}"))
}

/// Builds a line searcher honoring the context (`-A`/`-B`/`-C`) and invert
/// (`-v`) flags. Line numbers are always on (the grep line format needs them).
///
/// Crate-visible since ws43-02: shared with the hitstream engine's CLI-side
/// walk (see [`build_matcher`]).
pub fn build_searcher(flags: &GrepFlags) -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .before_context(flags.before_context)
        .after_context(flags.after_context)
        .invert_match(flags.invert)
        .build()
}

// ─── stdin (stream) mode ───────────────────────────────────────────────

/// Outcome of a plain-ripgrep pass over a stdin stream ([`grep_stream`]).
///
/// stdin mode carries the **same flags** as file mode but never enriches — a
/// stream has no file path or LSP context — so it produces plain ripgrep output:
/// matching (and context) lines verbatim, a count, or a single
/// `(standard input)` marker for `-l`.
pub enum StreamOutcome {
    /// Default / `-A`/`-B`/`-C` / `-v`: the selected lines verbatim, `\n`-joined
    /// (no trailing newline), in stream order. Empty string when nothing matched.
    Lines(String),
    /// `--count`: the number of matching (or, with `-v`, non-matching) lines.
    Count(usize),
    /// `-l`/`--files-with-matches`: whether the stream had at least one match.
    /// The CLI prints `(standard input)` when `true`, nothing when `false`
    /// (GNU `grep -l` convention for a stream with no filename).
    FilesWithMatches(bool),
}

/// Runs a plain ripgrep pass over an arbitrary stream — the `… | catenary grep
/// PAT` path.
///
/// No enrichment, no daemon, no `#scope`: a stream has no file or LSP context.
/// It carries the same flags as file mode (`-i`/`-s`/`-w`/`-F`/`-v`, context,
/// `--count`, `-l`), differing only in enrichment, never in capability.
///
/// # Errors
///
/// Returns an error if the pattern is invalid or the stream read fails.
pub fn grep_stream<R: std::io::Read>(
    reader: R,
    pattern: &str,
    flags: &GrepFlags,
    count: bool,
) -> Result<StreamOutcome> {
    let matcher = build_matcher(pattern, flags)?;
    let mut searcher = build_searcher(flags);
    let mut sink = StreamSink {
        lines: Vec::new(),
        match_count: 0,
    };
    searcher
        .search_reader(&matcher, reader, &mut sink)
        .map_err(|e| anyhow!("stdin search failed: {e}"))?;

    if count {
        Ok(StreamOutcome::Count(sink.match_count))
    } else if flags.files_with_matches {
        Ok(StreamOutcome::FilesWithMatches(sink.match_count > 0))
    } else {
        Ok(StreamOutcome::Lines(sink.lines.join("\n")))
    }
}

/// Collects selected lines (and context) from a stdin stream search.
///
/// `matched` fires for the selected lines (matching, or non-matching under `-v`)
/// and is what `match_count` tallies; `context` fires for `-A`/`-B`/`-C` lines,
/// which join the output but are never counted (matching ripgrep `--count`).
struct StreamSink {
    /// Selected and context lines, verbatim and newline-stripped, in stream order.
    lines: Vec<String>,
    /// Count of selected (matching, or inverted) lines — context excluded.
    match_count: usize,
}

impl StreamSink {
    /// Strips the trailing `\n` (and a CRLF `\r`) so a recorded line is
    /// byte-identical to what ripgrep prints.
    fn push_line(&mut self, bytes: &[u8]) {
        let raw = String::from_utf8_lossy(bytes);
        let trimmed = raw.strip_suffix('\n').unwrap_or(&raw);
        let line = trimmed.strip_suffix('\r').unwrap_or(trimmed);
        self.lines.push(line.to_string());
    }
}

impl Sink for StreamSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.match_count += 1;
        self.push_line(mat.bytes());
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        self.push_line(ctx.bytes());
        Ok(true)
    }
}

/// The displayed path for a hit: cwd-relative when a `cwd` is set (the normal
/// CLI case, like ripgrep), falling back to the verbatim absolute path when the
/// file is not under `cwd`; root-relative (via [`display_path`]) when no `cwd`
/// is supplied (the explicit all-roots mode).
fn rel_path(file: &Path, fs_manager: &FilesystemManager, cwd: Option<&Path>) -> String {
    cwd.map_or_else(
        || display_path(&file.to_string_lossy(), fs_manager),
        |base| {
            file.strip_prefix(base).map_or_else(
                |_| file.to_string_lossy().into_owned(),
                |rel| rel.to_string_lossy().into_owned(),
            )
        },
    )
}

/// Builds the `#scope` containment trail for a hit at `line_0` from the file's
/// `documentSymbol` ancestry: the chain of enclosing *named* symbols, outermost
/// → hit, each slugified and joined with a neutral `/`.
///
/// A **definition** drops its own leaf — a symbol whose own name line *is* the
/// hit line is the hit itself, so it is excluded (it fails `s.line < line_0`);
/// its ancestors remain. A **usage** keeps its innermost enclosing symbol (no symbol
/// starts on the hit line, so nothing is excluded). Anonymous scopes (closures,
/// blocks, `let`s) are absent from `documentSymbol`, so the trail is only ever
/// named scopes. Returns an empty `Vec` when the hit is genuinely top-level.
fn scope_trail(symbols: &[Symbol], line_0: u32) -> Vec<String> {
    // `s.line < line_0` both requires the symbol to start strictly above the hit
    // (so it encloses it) and drops the definition's own leaf — a symbol whose
    // name line *is* the hit line (`s.line == line_0`) is the hit itself.
    let mut chain: Vec<&Symbol> = symbols
        .iter()
        .filter(|s| s.line < line_0 && s.end_line >= line_0)
        .collect();
    // Outermost → innermost. Spans nest, so start line ascending is the
    // containment order; one symbol per line (the index dedups by start line),
    // with the wider span first on the (impossible) tie for total ordering.
    chain.sort_by(|a, b| {
        a.line
            .cmp(&b.line)
            .then_with(|| b.end_line.cmp(&a.end_line))
    });
    chain.iter().map(|s| slugify(&s.name)).collect()
}

/// Slugifies one scope-anchor segment: the grammar-significant characters
/// (whitespace and the scheme's own `/`, `:`, `#`) collapse to a single `-`,
/// runs collapsed, **case preserved**. A code identifier contains none of these,
/// so the transform is the identity on it; only free-text names (markdown
/// headings — `Pipeline Stages` → `Pipeline-Stages`) visibly change. Code and
/// markdown therefore share one path — "preserve when clean" is the fixed point
/// of this transform on identifier-shaped names, not a branch.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_whitespace() || matches!(ch, '/' | ':' | '#') {
            if !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        } else {
            out.push(ch);
            prev_dash = false;
        }
    }
    out
}

// ─── Skip records ──────────────────────────────────────────────────────

/// One file a grep walk skipped instead of searching.
///
/// Carries the absolute path, why it was skipped, and whether the path was
/// explicitly **named** (a file root — a positional arg or a glob that
/// expanded to it) versus reached by a directory walk. Folded into the
/// wire-ready [`GrepSkips`] by [`GrepSkips::from_records`] (misc 135, bug 62).
///
/// `pub` since ws43-02: the hitstream engine's CLI-side walk records these
/// (its [`WalkSummary`](crate::hitstream::engine::WalkSummary) carries them),
/// so a skip is reported identically to the retired executor's reporting.
#[derive(Debug)]
pub struct SkipRecord {
    /// Absolute path of the skipped file.
    pub path: PathBuf,
    /// Why the classifier treated it as unsearchable.
    pub reason: BinarySkip,
    /// The path was explicitly named (per-file reporting) vs walked (aggregated).
    pub named: bool,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use grep_matcher::Matcher;

    // ─── display_path tests ──────────────────────────────────────────────

    #[test]
    fn test_display_path_strips_root() {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            display_path("/home/user/project/src/main.rs", &fs),
            "src/main.rs"
        );
    }

    #[test]
    fn test_display_path_no_matching_root() {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            display_path("/other/path/file.rs", &fs),
            "/other/path/file.rs"
        );
    }

    // ─── slugify ─────────────────────────────────────────────────────────

    #[test]
    fn slugify_is_identity_on_code_identifiers() {
        // No whitespace/`/`/`:`/`#` → the transform passes the name through.
        assert_eq!(slugify("run"), "run");
        assert_eq!(slugify("DiagnosticFeeder"), "DiagnosticFeeder");
        assert_eq!(slugify("handle_grep"), "handle_grep");
    }

    #[test]
    fn slugify_collapses_significant_chars_preserving_case() {
        // whitespace / `/` / `:` / `#` → `-`, runs collapsed, case preserved.
        assert_eq!(slugify("Pipeline Stages"), "Pipeline-Stages");
        assert_eq!(slugify("a  b"), "a-b");
        assert_eq!(slugify("a:/#b"), "a-b");
        assert_eq!(slugify("Core/Sub"), "Core-Sub");
    }

    // ─── scope_trail + line-format helpers ─────────────────────────────────

    fn test_fs(root: &str) -> FilesystemManager {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from(root)]);
        fs
    }

    /// Build a `Symbol` spanning `[line, end_line]` with the given name.
    fn sym(name: &str, line: u32, end_line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: "function".to_string(),
            line,
            end_line,
            scope: None,
            scope_kind: None,
            deprecated: false,
        }
    }

    #[test]
    fn scope_trail_definition_drops_own_leaf() {
        // `foo` is defined at line 20 inside `impl Bar` (lines 18..=30). The hit
        // is foo's own name line, so its leaf is dropped — the trail is just the
        // container.
        let symbols = vec![sym("Bar", 18, 30), sym("foo", 20, 25)];
        assert_eq!(scope_trail(&symbols, 20), vec!["Bar".to_string()]);
    }

    #[test]
    fn scope_trail_usage_keeps_innermost() {
        // A usage at line 22 inside foo inside Bar — both kept, outermost first.
        let symbols = vec![sym("Bar", 18, 30), sym("foo", 20, 25)];
        assert_eq!(
            scope_trail(&symbols, 22),
            vec!["Bar".to_string(), "foo".to_string()]
        );
    }

    #[test]
    fn scope_trail_top_level_is_empty() {
        // A top-level definition (its own line) with no enclosing symbol → empty.
        let symbols = vec![sym("foo", 5, 9)];
        assert!(scope_trail(&symbols, 5).is_empty());
        // A line outside every symbol → empty.
        assert!(scope_trail(&symbols, 99).is_empty());
    }

    #[test]
    fn scope_trail_slugifies_and_orders_outermost_first() {
        // A markdown-style heading name slugifies; nesting joins outermost→inner.
        let symbols = vec![sym("Core", 0, 40), sym("Pipeline Stages", 10, 30)];
        assert_eq!(
            scope_trail(&symbols, 14),
            vec!["Core".to_string(), "Pipeline-Stages".to_string()]
        );
    }

    // ─── GrepSkips (misc 135 / bug 62) ────────────────────────────────────

    #[test]
    fn skips_empty_is_byte_identical_defaults() {
        // The all-searched common case: nothing to report, so the count suffix
        // is `None` (the count line is unchanged) and no skip lines render.
        let skips = GrepSkips::default();
        assert!(skips.is_empty());
        assert_eq!(skips.total(), 0);
        assert!(skips.count_suffix().is_none());
        assert!(skips.render_lines().is_empty());
    }

    #[test]
    fn skips_single_reason_count_suffix_names_it_plainly() {
        let skips = GrepSkips {
            named: vec![(
                "blob.bin".to_string(),
                BinarySkip::Binary.label().to_string(),
            )],
            walked: vec![],
        };
        assert_eq!(skips.total(), 1);
        assert_eq!(
            skips.count_suffix().as_deref(),
            Some(" (1 skipped: binary)")
        );
    }

    #[test]
    fn skips_render_lines_names_then_aggregates() {
        let skips = GrepSkips {
            named: vec![(
                "blob.bin".to_string(),
                BinarySkip::Binary.label().to_string(),
            )],
            walked: vec![(BinarySkip::Binary.label().to_string(), 1)],
        };
        assert_eq!(
            skips.render_lines(),
            vec![
                "skipped (binary): blob.bin".to_string(),
                "1 file skipped (binary)".to_string(),
            ]
        );
    }

    #[test]
    fn skips_from_records_splits_named_and_walked_dedup() {
        let fs = test_fs("/project");
        // Every skip is now content-binary (the size-cap reason retired, misc 140):
        // the named/walked split and dedup are unchanged.
        let records = vec![
            SkipRecord {
                path: PathBuf::from("/project/blob.bin"),
                reason: BinarySkip::Binary,
                named: true,
            },
            SkipRecord {
                path: PathBuf::from("/project/a.bin"),
                reason: BinarySkip::Binary,
                named: false,
            },
            SkipRecord {
                path: PathBuf::from("/project/b.bin"),
                reason: BinarySkip::Binary,
                named: false,
            },
            // Same path both named and walked → reported once, as named.
            SkipRecord {
                path: PathBuf::from("/project/blob.bin"),
                reason: BinarySkip::Binary,
                named: false,
            },
        ];
        let skips = GrepSkips::from_records(&records, &fs, Some(Path::new("/project")));
        assert_eq!(
            skips.named,
            vec![("blob.bin".to_string(), "binary".to_string())]
        );
        assert_eq!(skips.walked, vec![("binary".to_string(), 2)]);
        assert_eq!(skips.total(), 3);
    }

    // ─── flag parity: matcher (smart-case / -i / -s / -w / -F) ──────────

    /// Build a matcher from `flags` and test it against `hay`.
    fn matcher_hits(pattern: &str, flags: &GrepFlags, hay: &str) -> bool {
        let m = build_matcher(pattern, flags).expect("valid matcher");
        m.is_match(hay.as_bytes()).expect("is_match")
    }

    #[test]
    fn smart_case_is_the_default() {
        let f = GrepFlags::default();
        // Lowercase pattern → case-insensitive (matches mixed case).
        assert!(matcher_hits("config", &f, "let Config = 1;"));
        assert!(matcher_hits("config", &f, "config value"));
        // Pattern with an uppercase letter → case-sensitive.
        assert!(matcher_hits("Config", &f, "let Config = 1;"));
        assert!(!matcher_hits("Config", &f, "config value"));
    }

    #[test]
    fn ignore_case_forces_insensitive() {
        let f = GrepFlags {
            ignore_case: true,
            ..GrepFlags::default()
        };
        // Even an uppercase pattern matches lowercase text under `-i`.
        assert!(matcher_hits("Config", &f, "config value"));
    }

    #[test]
    fn case_sensitive_forces_sensitive() {
        let f = GrepFlags {
            case_sensitive: true,
            ..GrepFlags::default()
        };
        // A lowercase pattern no longer matches uppercase text under `-s`.
        assert!(!matcher_hits("config", &f, "Config value"));
        assert!(matcher_hits("config", &f, "config value"));
    }

    #[test]
    fn word_regexp_anchors_on_word_boundaries() {
        let f = GrepFlags {
            word: true,
            ..GrepFlags::default()
        };
        assert!(matcher_hits("cat", &f, "a cat sat"));
        assert!(!matcher_hits("cat", &f, "category"));
    }

    #[test]
    fn fixed_strings_treats_pattern_as_literal() {
        let fixed = GrepFlags {
            fixed_strings: true,
            ..GrepFlags::default()
        };
        // `.` is a literal dot, not "any char".
        assert!(matcher_hits("a.b", &fixed, "a.b"));
        assert!(!matcher_hits("a.b", &fixed, "axb"));
        // Without -F the same pattern is a regex (dot matches any char).
        assert!(matcher_hits("a.b", &GrepFlags::default(), "axb"));
    }

    // ─── stdin (stream) mode ────────────────────────────────────────────

    impl StreamOutcome {
        fn lines(self) -> Option<String> {
            if let Self::Lines(s) = self {
                Some(s)
            } else {
                None
            }
        }
        const fn count(&self) -> Option<usize> {
            if let Self::Count(n) = self {
                Some(*n)
            } else {
                None
            }
        }
        const fn files_with_matches(&self) -> Option<bool> {
            if let Self::FilesWithMatches(b) = self {
                Some(*b)
            } else {
                None
            }
        }
    }

    fn stream_lines(pattern: &str, flags: &GrepFlags, input: &str) -> String {
        grep_stream(input.as_bytes(), pattern, flags, false)
            .expect("grep_stream")
            .lines()
            .expect("Lines outcome")
    }

    #[test]
    fn stream_plain_matches_lines_verbatim() {
        let f = GrepFlags::default();
        assert_eq!(stream_lines("beta", &f, "alpha\nbeta\ngamma\n"), "beta");
        // No match → empty, no error.
        assert_eq!(stream_lines("zzz", &f, "alpha\nbeta\n"), "");
    }

    #[test]
    fn stream_carries_smart_case() {
        let f = GrepFlags::default();
        // Lowercase pattern → insensitive over the stream.
        assert_eq!(stream_lines("alpha", &f, "ALPHA\nbeta\n"), "ALPHA");
        // Uppercase pattern → sensitive.
        assert_eq!(stream_lines("Alpha", &f, "alpha\nbeta\n"), "");
    }

    #[test]
    fn stream_carries_invert_and_fixed_and_context() {
        let inverted = GrepFlags {
            invert: true,
            ..GrepFlags::default()
        };
        assert_eq!(
            stream_lines("beta", &inverted, "alpha\nbeta\ngamma\n"),
            "alpha\ngamma"
        );

        let fixed = GrepFlags {
            fixed_strings: true,
            ..GrepFlags::default()
        };
        assert_eq!(stream_lines("a.c", &fixed, "a.c\nabc\n"), "a.c");

        let after = GrepFlags {
            after_context: 1,
            ..GrepFlags::default()
        };
        assert_eq!(
            stream_lines("beta", &after, "alpha\nbeta\ngamma\n"),
            "beta\ngamma"
        );
    }

    #[test]
    fn stream_count_tallies_matching_lines() {
        let f = GrepFlags::default();
        let n = grep_stream(&b"a\nba\nc\n"[..], "a", &f, true)
            .expect("grep_stream")
            .count()
            .expect("Count outcome");
        assert_eq!(n, 2, "two lines contain 'a'");
    }

    #[test]
    fn stream_files_with_matches_reports_presence() {
        let f = GrepFlags {
            files_with_matches: true,
            ..GrepFlags::default()
        };
        let matched = grep_stream(&b"alpha\nbeta\n"[..], "beta", &f, false)
            .expect("grep_stream")
            .files_with_matches()
            .expect("FilesWithMatches outcome");
        assert!(matched, "stream matched");

        let missed = grep_stream(&b"alpha\nbeta\n"[..], "zzz", &f, false)
            .expect("grep_stream")
            .files_with_matches()
            .expect("FilesWithMatches outcome");
        assert!(!missed, "stream did not match");
    }
}
