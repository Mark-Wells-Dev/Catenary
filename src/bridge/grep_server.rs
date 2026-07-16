// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grep tool: ripgrep + symbol index pipeline with LSP enrichment.
//!
//! ## ws43 status (query streaming)
//!
//! The query-streaming rework (ws43) replaces this daemon-side executor with a
//! CLI-owned walk ([`crate::hitstream::engine`]) plus a daemon-side annotator
//! ([`super::hitstream_enricher::GrepHitEnricher`]). The enrichment itself has
//! already migrated (ws43-02): the executor and the annotator both run the
//! shared core below ([`anchor_context`], [`nudge_observed_files`]), so the two
//! cannot drift. This executor (`GrepServer::execute` and the `tool/grep`
//! dispatch arm) RETIRES for grep once the `catenary grep` CLI cutover
//! completes; it is kept live meanwhile because the CLI still calls it and the
//! integration suite drives `tool/grep` directly. Pieces shared with the glob
//! executor (`ensure_symbols`, `expand_search_paths`, the chunked framing)
//! stay until glob's own cutover (ws43-03).

use super::session::ExcludeSet;
use anyhow::{Context, Result, anyhow};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

use super::filesystem_manager::{
    BinarySkip, FilesystemManager, OBSERVED_STAT_MISS_MTIME, mtime_nanos, stat_with_retry,
};
use super::handler::display_path;
use crate::config::DispatchMethod;
use crate::lsp::server::LspServer;
use crate::lsp::{LspClientManager, WalkBreadth};
use crate::symbol_index::{ScopeFilter, Symbol, SymbolIndex};

/// Ripgrep-parity flags shared across the grep surfaces (`catenary grep`'s CLI,
/// the IPC [`crate::router::GrepRequest`], and this daemon-side [`GrepInput`]).
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

/// Input for grep tool.
#[derive(Debug, Deserialize)]
pub struct GrepInput {
    /// Search pattern (supports `|` for alternation, passed to ripgrep).
    pub pattern: String,
    /// Literal file/directory paths to scope the search.
    ///
    /// Each path is used as a direct root for the file walker — files
    /// are searched directly, directories are walked. No glob matching
    /// is applied. When empty, the search scopes to `cwd` or all
    /// workspace roots.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Glob patterns to exclude from matches (repeatable `--exclude-pattern`).
    ///
    /// Empty for a query with no exclusion. The router resolves each pattern
    /// against `cwd` before dispatch; a path is excluded when any matches.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Include gitignored files (default: false).
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files (default: false).
    #[serde(default)]
    pub include_hidden: bool,
    /// Working directory for cwd-scoped searches.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Return a match/file count instead of rendered results (default: false).
    ///
    /// A dumb, `grep -c`-style count taken straight from the ripgrep pass —
    /// no symbol classification, no LSP, no enrichment. Reports matching
    /// lines and distinct files, never a page.
    #[serde(default)]
    pub count: bool,
    /// Ripgrep-parity flags (case/word/fixed/invert/context/glob/type/`-l`),
    /// flattened so the wire stays a flat object.
    #[serde(flatten)]
    pub flags: GrepFlags,
}

/// A single grep hit, rendered as one self-contained URL-style deep-link line
/// `path:line#scope:<verbatim>`.
struct GrepHit {
    /// Absolute path of the matched file.
    file: PathBuf,
    /// 0-based line of the match.
    line: u32,
    /// The full source line at the hit, verbatim and newline-stripped — the
    /// `<verbatim>` payload, byte-identical to ripgrep (not the matched token).
    matched_text: String,
    /// The `#scope` graph coordinate for this hit (see [`Anchor`]).
    anchor: Anchor,
}

/// The `#scope` graph coordinate appended to a grep line — a containment trail,
/// not a resolvable path. The `#` scheme carries degradation natively (bug 48).
///
/// `pub(super)` since ws43-02: the hitstream annotator
/// ([`super::hitstream_enricher`]) maps this tri-state onto the wire
/// [`AnnotatedHit`](crate::hitstream::frame::AnnotatedHit) so the executor and
/// the annotator share one anchor semantics.
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

/// Outcome of a grep query.
///
/// Normal queries render the complete result to stdout; `--count`
/// (`GrepInput::count`) short-circuits to a numeric summary. Both carry the set
/// of files that were **skipped** rather than searched ([`GrepSkips`]) so a skip
/// is always reported, never silent (misc 135, bug 62).
pub enum GrepOutcome {
    /// The complete rendered output for stdout.
    Rendered {
        /// The complete rendered output for stdout, shaped into per-file hunks
        /// (misc 140 phase 2): small totals stay in memory (the hot path),
        /// oversized ones spill to a per-request spool file. Emitted hunk by
        /// hunk in global sort order, byte-identical either way.
        output: ShapedOutput,
        /// Files in the search scope skipped instead of searched.
        skipped: GrepSkips,
    },
    /// `--count` summary: a dumb `grep -c`-style tally from the ripgrep pass.
    Count {
        /// Number of matching lines (a line with multiple matches counts
        /// once, like `grep -c`).
        matches: usize,
        /// Number of distinct files holding a match.
        files: usize,
        /// Files in the search scope skipped instead of searched. A skip is
        /// never conflated with a no-match (`--count` reports it separately).
        skipped: GrepSkips,
    },
}

/// Files in the search scope that were skipped instead of searched (misc 135,
/// bug 62).
///
/// Carried alongside every grep outcome so a skip is reported, never silent.
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

    /// Folds the raw per-thread [`SkipRecord`]s from a ripgrep walk into the
    /// wire-ready named/walked split, resolving each path to its display form.
    /// Every path is counted once — a path both named and walked is reported as
    /// named (the stronger, per-file signal).
    fn from_records(
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

/// Grep tool server: ripgrep + symbol index pipeline with LSP enrichment.
pub struct GrepServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
}

impl GrepServer {
    /// Execute a grep query with the given parameters.
    ///
    /// `parent_id` is a UUID for LSP event correlation.
    /// `cancel` is triggered when the CLI client disconnects.
    pub async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<GrepOutcome> {
        let input: GrepInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        // No empty-pattern short-circuit: ripgrep parity means the empty pattern
        // matches every line, and `--count` on it is the line count (bug 83). The
        // matcher stack handles it — the regex engine fast-paths trivial patterns
        // and empty-pattern matching is O(1) per line, so no dedicated fast path
        // is warranted (maintainer ruling: a second code path for the degenerate
        // case would add upkeep for no measurable gain).

        // cwd-scoped search: present when no glob or relative glob.
        //
        // Canonicalize the incoming cwd at this ingestion seam: roots are
        // canonical (the daemon canonicalizes `CATENARY_ROOTS` and every
        // ephemeral mount), but the host passes its raw invoking directory. On a
        // symlinked-tempdir host (macOS `$TMPDIR` → `/private/var/…`, or any
        // symlinked prefix) the two spellings differ, so a raw cwd would make the
        // walk emit raw-spelled paths that fail `resolve_root`'s canonical prefix
        // check — the changed-set nudge then silently no-ops (no didChange, no
        // didChangeWatchedFiles). Canonicalizing here, once, keeps the walk,
        // `resolve_root`, the nudge, and the display `strip_prefix` all canonical-
        // to-canonical, matching the convention `handle_file_accumulation` and
        // `ensure_ephemeral_mounts` already follow. A not-yet-existing cwd keeps
        // its spelling (canonicalize can't resolve it).
        let cwd = input
            .cwd
            .as_ref()
            .map(|c| c.canonicalize().unwrap_or_else(|_| c.clone()));

        // Resolve path arguments into concrete search roots: existing paths
        // pass through, unexpanded glob patterns expand daemon-side via the
        // gitignore-aware walker. When path arguments were given but matched
        // nothing, the result is empty — never a fallback to a cwd-wide search.
        let search_paths = if input.paths.is_empty() {
            Vec::new()
        } else {
            let expanded = super::session::expand_search_paths(
                &input.paths,
                input.include_gitignored,
                input.include_hidden,
            );
            if expanded.is_empty() {
                return Ok(if input.count {
                    GrepOutcome::Count {
                        matches: 0,
                        files: 0,
                        skipped: GrepSkips::default(),
                    }
                } else {
                    GrepOutcome::Rendered {
                        output: ShapedOutput::empty(),
                        skipped: GrepSkips::default(),
                    }
                });
            }
            expanded
        };

        // Count mode is a dumb, `grep -c`-style tally: a single ripgrep pass
        // over the whole pattern, no alternation split, no symbol
        // classification, no LSP. Matching lines (a line counts once) and the
        // distinct files holding them, straight from the ripgrep result. Count
        // takes precedence over `-l` when both are given (the more specific
        // tally wins).
        if input.count {
            return self
                .count_matches(&input, &search_paths, cwd.as_deref(), cancel)
                .await;
        }

        // `-l`/`--files-with-matches`: a plain ripgrep pass, then just the
        // distinct matching files as cwd-relative paths (one per line) — no
        // enrichment, no `#scope`, no context (ripgrep drops context with `-l`).
        if input.flags.files_with_matches {
            return self
                .files_with_matches(&input, &search_paths, cwd.as_deref(), cancel)
                .await;
        }

        // Run the whole pattern in a single pass. Top-level `|` alternation is
        // handled natively by the ripgrep regex engine, so there is no arm
        // split: each match line is emitted exactly once (dedup), and the
        // per-arm root-header glitch from the findings is retired by
        // construction — the flat, header-free line format leaves nothing to
        // repeat per arm.
        let run_input = GrepInput {
            pattern: input.pattern.clone(),
            paths: search_paths,
            exclude: input.exclude.clone(),
            include_gitignored: input.include_gitignored,
            include_hidden: input.include_hidden,
            cwd: cwd.clone(),
            count: false,
            flags: input.flags.clone(),
        };
        let (output, skipped) = self
            .run(run_input, parent_id, cancel, cwd.as_deref())
            .await?;

        // The command's output is always complete (decision 025): print every
        // match. Grep lines are self-contained, so the host caps only the final
        // read at the end of a pipeline. Skipped-but-in-scope files ride along in
        // `skipped` so the completeness promise holds even for a file the walk
        // could not search (misc 135).
        Ok(GrepOutcome::Rendered { output, skipped })
    }

    /// Resolves the concrete filesystem roots a pathless (`.`/cwd-scoped) or
    /// path-scoped grep walks — the single point that binds `.` to a root, so
    /// `count_matches` and [`Self::run`] can never drift (bug 31).
    ///
    /// - **Path arguments present** ⇒ those literal paths, verbatim.
    /// - **No path arguments, `cwd` present** ⇒ exactly `[cwd]`, the literal
    ///   invoking directory. A `.`-scoped grep searches the cwd and nothing
    ///   else: a *different* registered root is **never** substituted, even
    ///   when the cwd's own root has no language server or its server is not
    ///   yet ready (raw ripgrep matches are LSP-independent, so the correct
    ///   root is always walked; LSP coverage is decided separately, for
    ///   labeling, via [`FilesystemManager::resolve_root`]). This is the fix
    ///   for the silent wrong-root false-negative in bug 31.
    /// - **No path arguments, `cwd` absent** ⇒ all registered workspace roots.
    ///   This is the deliberate "search everywhere" mode used when the caller
    ///   genuinely has no working directory (e.g. test fixtures); it is **not**
    ///   a fallback that masquerades as a `.`-scoped search. The CLI always
    ///   supplies `cwd`, so a real `.` grep never reaches this arm. Each root's
    ///   matches are rendered under its own header (and labeled `(no LSP)` when
    ///   uncovered), so the result never reads as a single cwd-scoped answer.
    fn effective_search_roots(&self, paths: &[PathBuf], cwd: Option<&Path>) -> Vec<PathBuf> {
        if paths.is_empty() {
            cwd.map_or_else(
                || self.client_manager.roots(),
                |cwd| vec![cwd.to_path_buf()],
            )
        } else {
            paths.to_vec()
        }
    }

    /// Dumb `grep -c`-style count: one ripgrep pass, tally matching lines and
    /// distinct files.
    ///
    /// Deliberately skips alternation splitting, symbol classification, LSP
    /// readiness, and enrichment — a count is a cheap, deterministic "how many
    /// lines match" answer, not the symbol-aware tree. A line with multiple
    /// matches counts once (`file_line_texts` is keyed by line), matching
    /// `grep -c`. `cwd` and `search_paths` scope the walk exactly as
    /// [`Self::run`] does.
    async fn count_matches(
        &self,
        input: &GrepInput,
        search_paths: &[PathBuf],
        cwd: Option<&Path>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<GrepOutcome> {
        let effective_roots = self.effective_search_roots(search_paths, cwd);
        let resolved_exclude = Arc::new(ExcludeSet::compile(&input.exclude)?);

        // A count must never inflate from context lines, and `-l` is irrelevant
        // here — keep only the matcher/file-filter flags, drop context.
        let flags = GrepFlags {
            before_context: 0,
            after_context: 0,
            files_with_matches: false,
            ..input.flags.clone()
        };
        let rg = Self::ripgrep_matches_blocking(
            input.pattern.clone(),
            effective_roots,
            resolved_exclude,
            input.include_gitignored,
            input.include_hidden,
            Arc::clone(&self.fs_manager),
            flags,
            cancel.clone(),
        )
        .await?;

        let matches: usize = rg.file_line_texts.values().map(HashMap::len).sum();
        let files = rg.file_line_texts.len();
        // A skip is not a no-match: report it separately so `--count` never
        // conflates the two (misc 135, bug 62).
        let skipped = GrepSkips::from_records(&rg.skips, &self.fs_manager, cwd);
        Ok(GrepOutcome::Count {
            matches,
            files,
            skipped,
        })
    }

    /// `-l`/`--files-with-matches`: a plain ripgrep pass, then the distinct
    /// matching files rendered as cwd-relative paths, one per line, sorted for
    /// byte-stable output. No enrichment, no `#scope`, no context (ripgrep drops
    /// context with `-l`) — the complete list prints, and a path per line keeps
    /// it pipe-safe (`-l` composes with `| head`/`| grep`).
    async fn files_with_matches(
        &self,
        input: &GrepInput,
        search_paths: &[PathBuf],
        cwd: Option<&Path>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<GrepOutcome> {
        let effective_roots = self.effective_search_roots(search_paths, cwd);
        let resolved_exclude = Arc::new(ExcludeSet::compile(&input.exclude)?);

        // Matcher/file-filter flags only; context never changes the file set.
        let flags = GrepFlags {
            before_context: 0,
            after_context: 0,
            files_with_matches: false,
            ..input.flags.clone()
        };
        let rg = Self::ripgrep_matches_blocking(
            input.pattern.clone(),
            effective_roots,
            resolved_exclude,
            input.include_gitignored,
            input.include_hidden,
            Arc::clone(&self.fs_manager),
            flags,
            cancel.clone(),
        )
        .await?;

        // Skips ride along even for `-l`: a named file skipped instead of
        // searched must not silently vanish from the file list (misc 135).
        let skipped = GrepSkips::from_records(&rg.skips, &self.fs_manager, cwd);

        if rg.file_lines.is_empty() {
            return Ok(GrepOutcome::Rendered {
                output: ShapedOutput::empty(),
                skipped,
            });
        }

        let mut paths: Vec<String> = rg
            .file_lines
            .keys()
            .map(|f| rel_path(Path::new(f), &self.fs_manager, cwd))
            .collect();
        paths.sort();
        let output = paths.join("\n");

        // `-l` output is a small path list — a single in-memory hunk, never
        // spooled. Wrapped so it streams over the same chunked framing as the
        // enriched path (misc 140 phase 2).
        Ok(GrepOutcome::Rendered {
            output: ShapedOutput::from_string(output),
            skipped,
        })
    }

    /// Grep pipeline: ripgrep + `documentSymbol` index + hit classification.
    #[allow(clippy::too_many_lines, reason = "Core grep orchestration")]
    async fn run(
        &self,
        input: GrepInput,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
        cwd: Option<&Path>,
    ) -> Result<(ShapedOutput, GrepSkips)> {
        debug!("Grep request: pattern={}", input.pattern);

        // All paths are literal — no glob interpretation. When no paths are
        // provided, bind to the invoking cwd (never another root) or, when the
        // caller has no cwd, the explicit all-roots mode. See
        // [`Self::effective_search_roots`] (bug 31).
        let effective_roots = self.effective_search_roots(&input.paths, cwd);
        let resolved_exclude = Arc::new(ExcludeSet::compile(&input.exclude)?);

        // Step 1: Ripgrep scoped to file set → raw hits with matched text.
        // Context lines (`-A`/`-B`/`-C`) and inverted selection (`-v`) are
        // captured here too — each becomes a hit and is anchored by containment
        // exactly like a match line. The synchronous parallel walk runs on a
        // blocking thread so the router's disconnect `select!` stays pollable and
        // the cancel token can actually fire mid-walk (misc 140).
        let rg = Self::ripgrep_matches_blocking(
            input.pattern.clone(),
            effective_roots.clone(),
            resolved_exclude.clone(),
            input.include_gitignored,
            input.include_hidden,
            Arc::clone(&self.fs_manager),
            input.flags.clone(),
            cancel.clone(),
        )
        .await?;

        // Fold the walk's skip records (built before any early return) so a
        // skip-only search — the bug-62 case, every match hidden behind a
        // skipped file — still reports the skip instead of empty silence.
        let skipped = GrepSkips::from_records(&rg.skips, &self.fs_manager, cwd);

        if rg.file_lines.is_empty() {
            return Ok((ShapedOutput::empty(), skipped));
        }

        // Step 2: Ensure servers exist for matched files and wait for readiness.
        // The wait is bounded (misc 197): a wedged/busy settle must never make
        // grep go silent. Past the bound the hits below serve unenriched (their
        // `#?` could-not-enrich anchor) — the ripgrep matches are already
        // complete, only the `#scope` annotation degrades.
        let rg_paths: Vec<PathBuf> = rg.file_lines.keys().map(PathBuf::from).collect();
        self.client_manager
            .ensure_and_wait_for_paths_bounded(
                &rg_paths,
                crate::lsp::manager::QUERY_ENRICHMENT_BUDGET,
            )
            .await;

        // Step 2a: Route the changed-set nudge (WS31 Consumer A). The shared
        // [`nudge_observed_files`] carries the walk-breadth gate and the reap
        // rules; the executor's contribution is the reap eligibility: only a
        // pathless grep — whose walked scopes (canonicalized, so they compare
        // against the canonicalized roots `resolve_root` returns) may cover a
        // whole registered root — passes scopes at all. `--count` grep never
        // reaches `run`, so it pays nothing. No edited-set exclusion for grep.
        let reap_scopes: Option<Vec<PathBuf>> = input.paths.is_empty().then(|| {
            effective_roots
                .iter()
                .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                .collect()
        });
        nudge_observed_files(
            &self.client_manager,
            &self.fs_manager,
            &rg.files,
            reap_scopes.as_deref(),
        )
        .await;

        // Steps 2b–3: symbol-index population plus per-file anchor coverage —
        // the shared enrichment core ([`anchor_context`], ws43-02) the hitstream
        // annotator also runs, so the executor and the annotator cannot drift.
        let anchors = anchor_context(
            self.symbol_index.as_ref(),
            &self.client_manager,
            &self.fs_manager,
            &rg_paths,
            parent_id,
        )
        .await;

        if cancel.is_cancelled() {
            return Err(crate::mcp::RequestCancelled.into());
        }

        // Step 4: build one self-contained line per hit — `path:line#scope:raw`.
        // Every ripgrep match is a top-level line (hits-as-spine); the `#scope`
        // anchor is the containment trail from documentSymbol ancestry. No match
        // is ever dropped (strict ripgrep superset).
        let mut hits: Vec<GrepHit> = Vec::new();
        for (file_str, line_map) in &rg.file_line_texts {
            let file_path = PathBuf::from(file_str);
            for (&line_1, texts) in line_map {
                let line_0 = line_1 - 1;
                let matched_text = texts.first().map(|(t, _)| t.clone()).unwrap_or_default();
                hits.push(GrepHit {
                    file: file_path.clone(),
                    line: line_0,
                    matched_text,
                    anchor: anchors.anchor_for(&file_path, line_0),
                });
            }
        }

        if hits.is_empty() {
            return Ok((ShapedOutput::empty(), skipped));
        }

        // Shape the hits into per-file hunks, buffering small totals in memory
        // (the hot path) and spilling above the threshold to a per-request spool
        // file — bounded shaping memory, invisible to the output contract (misc
        // 140 phase 2, decision 029 §5). Emission (the router) iterates the map
        // in key order, preserving the deterministic global (file, line) sort.
        let shaped = shape_hits(&hits, &self.fs_manager, cwd, GREP_SPOOL_THRESHOLD)?;
        Ok((shaped, skipped))
    }

    /// Runs [`Self::ripgrep_matches`] on a blocking thread.
    ///
    /// The walk is a synchronous `walker.run()` (ripgrep's parallel walker). Left
    /// on an async runtime worker it would pin that thread for the whole walk, so
    /// the router's disconnect-`select!` could never poll its cancel branch — the
    /// walk would read a dead client's tree to completion (misc 140 audit §4).
    /// Moving it to [`tokio::task::spawn_blocking`] keeps the runtime free to fire
    /// the cancel token, which the walker visitors observe per file and answer
    /// with `WalkState::Quit`. Dropping the join handle on cancel detaches the
    /// blocking task; the threaded token is what actually stops its work promptly.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid regex, or if the blocking
    /// walk task panics.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors ripgrep_matches, owned for the blocking boundary"
    )]
    async fn ripgrep_matches_blocking(
        pattern: String,
        roots: Vec<PathBuf>,
        exclude: Arc<ExcludeSet>,
        include_gitignored: bool,
        include_hidden: bool,
        fs_manager: Arc<FilesystemManager>,
        flags: GrepFlags,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<RipgrepMatches> {
        tokio::task::spawn_blocking(move || {
            Self::ripgrep_matches(
                &pattern,
                &roots,
                &exclude,
                include_gitignored,
                include_hidden,
                &fs_manager,
                &flags,
                &cancel,
            )
        })
        .await
        .map_err(|e| anyhow!("grep walk task failed: {e}"))?
    }

    /// Searches workspace roots for pattern matches using the `grep-*` crates
    /// (ripgrep's internals). Walks files in parallel and returns matched
    /// strings and per-file line numbers in a single pass per file.
    ///
    /// `cancel` is threaded into every parallel walker visitor: a fired token
    /// (the CLI client disconnected) quits the walk at the next file rather than
    /// reading the tree to completion (misc 140). Runs synchronously — callers on
    /// the async runtime go through [`Self::ripgrep_matches_blocking`].
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid regex.
    #[allow(
        clippy::too_many_lines,
        clippy::too_many_arguments,
        reason = "Single-pass parallel walk + skip recording; cancel token threaded in"
    )]
    fn ripgrep_matches(
        pattern: &str,
        roots: &[PathBuf],
        exclude: &Arc<ExcludeSet>,
        include_gitignored: bool,
        include_hidden: bool,
        fs_manager: &Arc<FilesystemManager>,
        flags: &GrepFlags,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<RipgrepMatches> {
        use ignore::WalkState;
        use std::sync::Mutex as StdMutex;

        let matcher = build_matcher(pattern, flags)?;

        // `-t`/`--type` resolves against ripgrep's built-in type definitions and
        // is root-independent, so build it once and clone per directory walk.
        let types = if flags.types.is_empty() {
            None
        } else {
            let mut tb = TypesBuilder::new();
            tb.add_defaults();
            for t in &flags.types {
                tb.select(t);
            }
            Some(
                tb.build()
                    .map_err(|e| anyhow!("Invalid --type filter: {e}"))?,
            )
        };

        let collected = Arc::new(StdMutex::new(Vec::<ThreadMatches>::new()));

        // WalkBuilder flags use "skip" semantics: .hidden(true) = skip hidden
        let skip_gitignored = !include_gitignored;
        let skip_hidden = !include_hidden;

        for root in roots {
            // An explicitly-named path that resolves to a file is a direct
            // request to search that exact file (misc 110, ripgrep parity), so
            // gitignore/hidden filtering must not gate it — those rules govern
            // recursive *directory* traversal, not paths the user named. Bypass
            // the gate for a file root; keep it for directory walks.
            let root_is_file = root.is_file();
            let mut builder = WalkBuilder::new(root);
            builder
                .git_ignore(skip_gitignored && !root_is_file)
                .hidden(skip_hidden && !root_is_file);
            // Positive file filters (`-g`/`--type`) govern recursive directory
            // traversal, not an explicitly-named file (same misc-110 reasoning as
            // the gitignore/hidden bypass): naming a file is a direct request for
            // it, so a `-g`/`--type` whitelist never silently drops it.
            if !root_is_file {
                if !flags.globs.is_empty() {
                    let mut ob = OverrideBuilder::new(root);
                    for g in &flags.globs {
                        ob.add(g)
                            .map_err(|e| anyhow!("Invalid --glob pattern '{g}': {e}"))?;
                    }
                    builder.overrides(
                        ob.build()
                            .map_err(|e| anyhow!("Invalid --glob filter: {e}"))?,
                    );
                }
                if let Some(types) = &types {
                    builder.types(types.clone());
                }
            }
            let walker = builder.build_parallel();

            walker.run(|| {
                let matcher = matcher.clone();
                let mut searcher = build_searcher(flags);
                let invert = flags.invert;
                let exclude = Arc::clone(exclude);
                let root = root.clone();
                // A file root is an explicitly-named path (positional arg or a
                // glob that expanded to it); its skip is reported per-file. A
                // directory root's files are unnamed and aggregate (misc 135).
                // Copied into a distinct local so the inner `move` closure owns
                // it (mirroring `invert` above).
                let named_root = root_is_file;
                let fs_manager = Arc::clone(fs_manager);
                // Per-thread cancel handle (the token is cheap to clone — Arc
                // inside). A fired token quits this thread's walk (misc 140).
                let cancel = cancel.clone();
                let mut state = CollectOnDrop {
                    local: ThreadMatches::default(),
                    collected: Arc::clone(&collected),
                };

                Box::new(move |entry| {
                    // Real walk cancellation (misc 140): the router fires this
                    // token when the CLI client disconnects. Quit the parallel
                    // walk at the first entry a cancelled thread reaches instead
                    // of reading the tree to completion — the incident this guard
                    // closes (a terabyte-scale walk for a client that is gone).
                    if cancel.is_cancelled() {
                        return WalkState::Quit;
                    }
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    let path = entry.path();
                    // Directories carry no matches and are not searched.
                    if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                        return WalkState::Continue;
                    }
                    // File decision: trust the walker's cached `d_type` (no
                    // fresh stat). `DirEntry::file_type()` is `None` only for
                    // stdin, which this filesystem walker never yields, so the
                    // type is always known here — no re-stat (and no transient
                    // miss to retry): the cached `d_type` is exactly what fixes
                    // the rename race (bug 34/35) by never re-statting.
                    //
                    // A *traversed* symlink-to-file is reported by the `ignore`
                    // walker with its **own** type (`is_file()==false`), so it is
                    // skipped here by default — ripgrep parity (`-L` off). The
                    // skip is intentional: an in-tree symlink target is still
                    // searched via its real path (following it would yield
                    // duplicate matches under both paths), and the only gap (a
                    // target outside the walked set) is opt-in via
                    // `--follow-links` (planned, fs-coherence ticket 07).
                    // Explicitly-named symlink args are unaffected (the root
                    // entry follows and stores the target type).
                    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                        // A non-file entry (directory handled above, plus
                        // sockets, broken or traversed symlinks) — debug, not a
                        // user-facing warning.
                        debug!("grep: skipping non-file entry {}", path.display());
                        return WalkState::Continue;
                    }

                    // Record this file's mtime for the WS31 changed-set baseline
                    // (Consumer A) — every visited file, before the query-level
                    // `exclude` and binary skips, so coherence coverage is the
                    // full tree (the manager scopes it to registered globs). The
                    // metadata is retried (a fresh stat can race an atomic rename
                    // even when `d_type` already proved the entry a file) and
                    // reused by the binary check below.
                    //
                    // An enumerated present file whose stat still misses is
                    // recorded with the `OBSERVED_STAT_MISS_MTIME` sentinel, NOT
                    // omitted: omitting it would drop it from the observation set
                    // and a full walk would then false-reap it as `Deleted`
                    // (WS31-review H1). A stat-miss must never reach the reap set.
                    let metadata = stat_with_retry(path);
                    let observed_mtime = metadata
                        .as_ref()
                        .map_or(OBSERVED_STAT_MISS_MTIME, mtime_nanos);
                    state.local.files.push((path.to_path_buf(), observed_mtime));

                    if exclude.is_match(path, &root) {
                        return WalkState::Continue;
                    }

                    // Skip a file the classifier treats as binary — a NUL byte
                    // before any text BOM. Classification is now purely
                    // content-based at any size (misc 140, decision 029): the
                    // former size cap that skipped large *text* files unread is
                    // gone, so a 15.7 MB pure-UTF-8 bundle is searched to EOF, not
                    // skipped (bug 62). Rather than drop a genuinely binary file
                    // silently — the old `0 matches in 0 files` indistinguishable
                    // from a true no-match — record the skip with its reason and
                    // whether it was explicitly named, so the outcome reports it
                    // (never silence, misc 135).
                    if let Some(md) = &metadata
                        && let Some(reason) = fs_manager.binary_skip_reason(path, md)
                    {
                        state.local.skips.push(SkipRecord {
                            path: path.to_path_buf(),
                            reason,
                            named: named_root,
                        });
                        return WalkState::Continue;
                    }

                    let path_str = path.to_string_lossy().to_string();
                    let mut sink = MatchSink {
                        matcher: &matcher,
                        path: &path_str,
                        local: &mut state.local,
                        invert,
                    };

                    if let Err(e) = searcher.search_path(&matcher, path, &mut sink) {
                        debug!("grep: skipping {path_str}: {e}");
                    }

                    WalkState::Continue
                })
            });
        }

        let parts = harvest(collected)?;

        Ok(RipgrepMatches::merge(parts))
    }

    /// Builds the ws43 hit-batch enricher from this server's shared
    /// infrastructure (pool, filesystem manager, symbol index) — the daemon-side
    /// annotator for the streamed `catenary grep` engine. The enricher runs the
    /// same shared core ([`anchor_context`], [`nudge_observed_files`]) this
    /// executor runs, so the two paths cannot drift while they coexist.
    #[must_use]
    pub fn hitstream_enricher(&self) -> super::hitstream_enricher::GrepHitEnricher {
        super::hitstream_enricher::GrepHitEnricher::new(
            Arc::clone(&self.client_manager),
            Arc::clone(&self.fs_manager),
            self.symbol_index.clone(),
        )
    }
}

// ─── Shared enrichment core (executor + ws43 hitstream annotator) ────────
//
// The pieces below are the single implementation of grep's LSP enrichment,
// called by BOTH the legacy query executor (`GrepServer::run`, above) and the
// streamed-engine annotator (`super::hitstream_enricher`). When the `catenary
// grep` CLI cutover completes, the executor path retires and these become the
// annotator's alone; until then, living here — beside the executor that used to
// inline them — keeps the two callers on one implementation (ws43-02: move
// logic, don't duplicate it).

/// Per-file enrichment context for a set of matched paths: the `documentSymbol`
/// outlines (the sole source of the `#scope` anchor) plus the set of files that
/// could not be enriched at all.
///
/// Built once per query (executor) or once per hit-batch (annotator) by
/// [`anchor_context`]; consumed per hit via [`Self::anchor_for`].
pub(super) struct AnchorContext {
    /// `documentSymbol` outlines per matched file.
    file_symbols: HashMap<PathBuf, Vec<Symbol>>,
    /// Files with no `documentSymbol` coverage (no live capable server): their
    /// hits carry the `#?` could-not-enrich marker.
    uncovered: HashSet<PathBuf>,
}

impl AnchorContext {
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
/// file the caller's walk visited (executor) or the canonical hit paths of one
/// batch, freshly statted (annotator). They are grouped by registered root
/// (root-relative), diffed against the per-root baseline, and the delta routed
/// per server. A root with no covering server is `WalkBreadth::None` — the
/// `(no LSP)` case — and is skipped entirely (no diff, no nudge).
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

// ─── Rendering ─────────────────────────────────────────────────────────

/// Renders one grep hit as its self-contained deep-link line into `out`,
/// terminated by a newline: `path:line#scope:<verbatim>`.
///
/// `rel` is the hit's precomputed display path ([`rel_path`]). The `#scope`
/// fragment is omitted when the hit is top-level and `#?` when it could not be
/// enriched; stripping it (up to the next `:`) yields byte-exact ripgrep
/// (`path:line:text`) — the superset contract. The single source of the grep
/// line format, shared by [`shape_hits`] (production) and `render_results` (the
/// byte-identity test oracle) so the two can never drift.
fn render_hit_line(out: &mut String, hit: &GrepHit, rel: &str) {
    use std::fmt::Write;

    let line_1 = hit.line + 1;
    match &hit.anchor {
        Anchor::Scope(trail) => {
            let _ = writeln!(out, "{rel}:{line_1}#{trail}:{}", hit.matched_text);
        }
        Anchor::TopLevel => {
            let _ = writeln!(out, "{rel}:{line_1}:{}", hit.matched_text);
        }
        Anchor::Unknown => {
            let _ = writeln!(out, "{rel}:{line_1}#?:{}", hit.matched_text);
        }
    }
}

/// Renders grep hits as one self-contained URL-style deep-link line each —
/// `path:line#scope:<verbatim>` — ordered by `(file, line)` for byte-stable
/// output (the misc-32 determinism pattern).
///
/// The single-string reference renderer: production streams the equivalent
/// bytes hunk by hunk via [`shape_hits`], and the byte-identity test asserts the
/// two agree. There is **no header** of any kind: every line carries its own
/// cwd-relative path, exactly like ripgrep, so the output is pipe-safe. The
/// verbatim payload floats (variable start column, indentation preserved) with
/// no padding. Returns the complete output (decision 025) — every match, no
/// volume branch.
#[cfg(test)]
fn render_results(hits: &[GrepHit], fs_manager: &FilesystemManager, cwd: Option<&Path>) -> String {
    let mut ordered: Vec<&GrepHit> = hits.iter().collect();
    ordered.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));

    let mut out = String::new();
    for hit in ordered {
        let rel = rel_path(&hit.file, fs_manager, cwd);
        render_hit_line(&mut out, hit, &rel);
    }

    let trimmed_len = out.trim_end().len();
    out.truncate(trimmed_len);
    out
}

// ─── Bounded shaping memory: per-file hunks + spool (misc 140 phase 2) ────

/// Default daemon-side buffer threshold (bytes). Rendered grep output up to this
/// total stays entirely in memory — the hot path, no disk I/O, no behavior
/// change; above it, later per-file hunks spill to a per-request spool file. The
/// threshold decides only *where the daemon buffers*, never *what the requester
/// receives* (decision 029 §5): output is byte-identical either way.
const GREP_SPOOL_THRESHOLD: usize = 1 << 20;

/// One rendered per-file grep hunk, either buffered in memory (the hot path) or
/// spilled to the per-request spool file at `[offset, offset + len)`.
enum Hunk {
    /// Small hunk kept in RAM.
    InMemory(String),
    /// Oversized hunk spilled to the spool file.
    Spooled {
        /// Byte offset of the hunk in the spool file.
        offset: u64,
        /// Byte length of the hunk.
        len: u64,
    },
}

/// One rendered grep hunk in global sort order, ready for the router to stream.
///
/// Either buffered in memory (the hot path) or a `[offset, offset + len)` slice
/// of the per-request spool file, read back through the [`HunkSpool`] handle
/// returned alongside it by [`ShapedOutput::into_parts`].
pub enum HunkChunk {
    /// Buffered in RAM — stream its bytes directly.
    InMemory(String),
    /// Spilled to the spool — read `len` bytes at `offset` from the spool file.
    Spooled {
        /// Byte offset of the hunk in the spool file.
        offset: u64,
        /// Byte length of the hunk.
        len: u64,
    },
}

/// The shaped grep output: per-file rendered hunks in global (file, line) sort
/// order, plus the per-request spool file (when any hunk overflowed the
/// in-memory threshold).
///
/// Peak memory is the path index plus one hunk in flight: the router iterates
/// [`into_parts`](Self::into_parts) in key order, streaming each hunk as a chunk
/// frame and reading spooled hunks one at a time. The spool is unlinked when
/// this value drops — on normal completion and on cancellation alike.
#[derive(Default)]
pub struct ShapedOutput {
    /// Rendered hunks keyed by file path — the key order *is* the deterministic
    /// global (file, line) sort (line order within a hunk is already correct).
    hunks: BTreeMap<PathBuf, Hunk>,
    /// The per-request spool, shared so a hunk read can move it into a blocking
    /// task; `None` when everything fit in memory (the hot path).
    spool: Option<Arc<HunkSpool>>,
}

impl ShapedOutput {
    /// An empty shaped output (no matches, or a `--count`/`-l` empty result).
    fn empty() -> Self {
        Self::default()
    }

    /// Wraps a single already-rendered string as one in-memory hunk — the `-l`
    /// (files-with-matches) path, whose small path list is never spooled. An
    /// empty string yields no hunk.
    fn from_string(s: String) -> Self {
        let mut hunks = BTreeMap::new();
        if !s.is_empty() {
            hunks.insert(PathBuf::new(), Hunk::InMemory(s));
        }
        Self { hunks, spool: None }
    }

    /// True when there is no rendered output to stream (no chunk frames).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Consumes the shaped output into its ordered hunks (global sort order) plus
    /// the shared spool handle. In-memory hunks carry their bytes; spooled hunks
    /// carry `[offset, len)` and are read back through the returned spool.
    #[must_use]
    pub fn into_parts(self) -> (Vec<HunkChunk>, Option<Arc<HunkSpool>>) {
        let chunks = self
            .hunks
            .into_values()
            .map(|h| match h {
                Hunk::InMemory(s) => HunkChunk::InMemory(s),
                Hunk::Spooled { offset, len } => HunkChunk::Spooled { offset, len },
            })
            .collect();
        (chunks, self.spool)
    }

    /// Reads every hunk (in-memory and spooled) in sort order, concatenates,
    /// and trims the trailing newline — exactly what the CLI reconstructs from
    /// the chunk stream. The byte-identity oracle for the spool path.
    #[cfg(test)]
    #[allow(clippy::expect_used, reason = "test-only oracle helper")]
    fn materialize(&self) -> String {
        let mut out = String::new();
        for hunk in self.hunks.values() {
            match hunk {
                Hunk::InMemory(s) => out.push_str(s),
                Hunk::Spooled { offset, len } => {
                    let spool = self.spool.as_ref().expect("spooled hunk needs a spool");
                    let bytes = spool.read_hunk(*offset, *len).expect("read spooled hunk");
                    out.push_str(&String::from_utf8(bytes).expect("spool hunk is utf-8"));
                }
            }
        }
        out.truncate(out.trim_end().len());
        out
    }

    /// The spool file path, when any hunk spilled — for lifecycle assertions.
    #[cfg(test)]
    fn spool_path(&self) -> Option<PathBuf> {
        self.spool.as_ref().map(|s| s.path.clone())
    }
}

/// Per-request disk-backed spool for grep hunks that overflow the in-memory
/// buffer threshold (misc 140 phase 2).
///
/// Lives under [`cache_dir`](crate::paths::cache_dir) — regenerable — never
/// [`runtime_dir`](crate::paths::runtime_dir), whose tmpfs backing would make
/// the spool RAM and the guard a no-op. The file is unlinked on drop, so it is
/// removed on both normal completion and cancellation (the owning
/// [`ShapedOutput`] drops on either path). Writes during shaping are sequential
/// through the owned handle; reads during emission open a fresh handle and seek,
/// so the spool can be shared behind an `Arc` for a blocking read.
pub struct HunkSpool {
    /// Sequential write handle, used only while shaping appends hunks.
    writer: std::fs::File,
    /// The spool file path (unlinked on drop; reopened per read).
    path: PathBuf,
    /// Bytes written so far — the offset of the next appended hunk.
    len: u64,
}

impl HunkSpool {
    /// Creates a fresh, uniquely-named spool file under `cache_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if the spool directory cannot be created or the file
    /// cannot be opened.
    fn create() -> Result<Self> {
        let dir = crate::paths::cache_dir()
            .join("catenary")
            .join("grep-spool");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create grep spool dir: {}", dir.display()))?;
        let path = dir.join(format!("{}.spool", uuid::Uuid::new_v4()));
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("create grep spool file: {}", path.display()))?;
        Ok(Self {
            writer,
            path,
            len: 0,
        })
    }

    /// Appends one hunk's bytes, returning its `(offset, len)` in the spool.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    fn append(&mut self, bytes: &[u8]) -> Result<(u64, u64)> {
        use std::io::Write;
        let offset = self.len;
        self.writer
            .write_all(bytes)
            .with_context(|| format!("append to grep spool: {}", self.path.display()))?;
        let len = bytes.len() as u64;
        self.len += len;
        Ok((offset, len))
    }

    /// Flushes buffered writes so a fresh read handle sees every appended hunk.
    ///
    /// # Errors
    ///
    /// Returns an error if the flush fails.
    fn finish(&mut self) -> Result<()> {
        use std::io::Write;
        self.writer
            .flush()
            .with_context(|| format!("flush grep spool: {}", self.path.display()))
    }

    /// Reads back one hunk's bytes: `len` bytes at `offset`.
    ///
    /// Opens a fresh read handle and seeks, so this needs only `&self` and can
    /// run inside a blocking task against an `Arc`-shared spool.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened, sought, or read.
    pub fn read_hunk(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut reader = std::fs::File::open(&self.path)
            .with_context(|| format!("open grep spool for read: {}", self.path.display()))?;
        reader
            .seek(SeekFrom::Start(offset))
            .with_context(|| format!("seek grep spool: {}", self.path.display()))?;
        let mut buf = vec![0u8; usize::try_from(len).context("spool hunk length overflows usize")?];
        reader
            .read_exact(&mut buf)
            .with_context(|| format!("read grep spool hunk: {}", self.path.display()))?;
        Ok(buf)
    }
}

impl Drop for HunkSpool {
    fn drop(&mut self) {
        // Best-effort per-request cleanup: the spool is regenerable ephemera, so
        // a failed unlink is a debug note, not a user-facing warning.
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            debug!("grep spool cleanup failed for {}: {e}", self.path.display());
        }
    }
}

/// Shapes rendered grep hits into per-file hunks, buffering small totals in
/// memory (the hot path) and spilling above `threshold` bytes to a per-request
/// spool file (misc 140 phase 2, decision 029 §5).
///
/// Hits are grouped by file — shaping is file-local (audit §2) — and each file's
/// hunk is rendered in ascending line order via [`render_hit_line`]. The
/// `BTreeMap<PathBuf, _>` key order reproduces the exact `(file, line)` global
/// sort [`render_results`] produces, so concatenating the hunks in key order and
/// trimming the trailing newline is byte-identical to the single-string render —
/// the invariant the byte-identity test pins. The running in-memory total is
/// what the threshold gates: once it would exceed `threshold`, later hunks spill.
///
/// # Errors
///
/// Returns an error if the spool file cannot be created or written.
fn shape_hits(
    hits: &[GrepHit],
    fs_manager: &FilesystemManager,
    cwd: Option<&Path>,
    threshold: usize,
) -> Result<ShapedOutput> {
    let mut by_file: BTreeMap<PathBuf, Vec<&GrepHit>> = BTreeMap::new();
    for hit in hits {
        by_file.entry(hit.file.clone()).or_default().push(hit);
    }

    let mut hunks: BTreeMap<PathBuf, Hunk> = BTreeMap::new();
    let mut spool: Option<HunkSpool> = None;
    let mut in_memory_bytes: usize = 0;

    for (file, mut file_hits) in by_file {
        // Line order within a file is the second sort key; one hit per line
        // (built one per `(file, line)` in `run`), so this is a total order.
        file_hits.sort_by_key(|h| h.line);
        let rel = rel_path(&file, fs_manager, cwd);
        let mut block = String::new();
        for hit in file_hits {
            render_hit_line(&mut block, hit, &rel);
        }

        if in_memory_bytes.saturating_add(block.len()) <= threshold {
            in_memory_bytes = in_memory_bytes.saturating_add(block.len());
            hunks.insert(file, Hunk::InMemory(block));
        } else {
            if spool.is_none() {
                spool = Some(HunkSpool::create()?);
            }
            let sp = spool
                .as_mut()
                .ok_or_else(|| anyhow!("grep spool missing after creation"))?;
            let (offset, len) = sp.append(block.as_bytes())?;
            hunks.insert(file, Hunk::Spooled { offset, len });
        }
    }

    if let Some(sp) = spool.as_mut() {
        sp.finish()?;
    }

    Ok(ShapedOutput {
        hunks,
        spool: spool.map(Arc::new),
    })
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

/// Wrapper that pushes per-thread match data into a shared collector on drop.
/// Each parallel walker thread owns one of these; when `run()` returns and the
/// closures are dropped, each thread's accumulated matches are flushed.
///
/// The poison recovery in [`Drop::drop`] (and the matching
/// [`PoisonError::into_inner`](std::sync::PoisonError::into_inner) in
/// [`harvest`]) is a **test-profile safety net**: it only fires when a sibling
/// walker thread *unwinds*, which requires `panic = "unwind"`. The release
/// profile sets `panic = "abort"` (`Cargo.toml`), so a walker panic aborts the
/// whole daemon and this recovery never runs in production. Correctness in
/// release therefore relies on the walker closure being panic-free — it is: every
/// fallible op inside the closure is `Result`-handled. The recovery still earns
/// its keep under the (unwind) test profile, where a panicking test walker must
/// not silently discard the matches its siblings already pushed.
struct CollectOnDrop {
    local: ThreadMatches,
    collected: Arc<std::sync::Mutex<Vec<ThreadMatches>>>,
}

impl Drop for CollectOnDrop {
    fn drop(&mut self) {
        let local = std::mem::take(&mut self.local);
        // Flush when this thread saw any matches OR any files OR any skips: the
        // changed-set baseline (WS31) needs every visited file even from a thread
        // whose files held no pattern match, and a thread whose only work was a
        // skipped file must still surface that skip (misc 135). A skip always
        // implies a visited file, so the `skips` clause is belt-and-suspenders.
        if local.file_lines.is_empty() && local.files.is_empty() && local.skips.is_empty() {
            return;
        }
        // Recover a poisoned mutex rather than silently discard this thread's
        // matches — a panicked sibling thread must not lose our results.
        let mut vec = self
            .collected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vec.push(local);
    }
}

/// Collects per-file match data for the ripgrep library search.
struct MatchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    path: &'a str,
    local: &'a mut ThreadMatches,
    /// `-v`/`--invert-match`: the selected lines are the *non*-matching ones, so
    /// they carry no match column and `matcher.find` would (correctly) miss.
    invert: bool,
}

impl MatchSink<'_> {
    /// Records one line (a match, an inverted selection, or a context line) into
    /// the per-file accumulators: its newline-stripped verbatim text plus the
    /// first-match column (0 for inverted/context lines, which have no match).
    fn record(&mut self, line_num: u32, line_bytes: &[u8], col: u32) {
        let raw = String::from_utf8_lossy(line_bytes);
        // Strip the trailing newline (and a CRLF `\r`) so the atom is the line
        // text, byte-identical to what `rg` prints.
        let trimmed = raw.strip_suffix('\n').unwrap_or(&raw);
        let line_str = trimmed.strip_suffix('\r').unwrap_or(trimmed).to_string();

        self.local
            .file_line_texts
            .entry(self.path.to_string())
            .or_default()
            .entry(line_num)
            .or_default()
            .push((line_str, col));

        self.local
            .file_lines
            .entry(self.path.to_string())
            .or_default()
            .push(line_num);
    }
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let Some(line_num) = mat.line_number().and_then(|n| u32::try_from(n).ok()) else {
            return Ok(true);
        };

        let line_bytes = mat.bytes();

        // One-atom model (decision 024): the hit carries its FULL source line,
        // byte-identical to `rg`, not the matched token (`--only-matching` is
        // dropped). Capture the whole line, newline-stripped, plus the column
        // of the FIRST match on it — the column still positions `prepareRename`
        // (enrichment gating) and the enrichment query at the symbol. Under `-v`
        // the selected line does NOT match the pattern, so there is no column to
        // find — anchor at column 0.
        let col = if self.invert {
            0
        } else {
            let Some(first) = self.matcher.find(line_bytes).ok().flatten() else {
                return Ok(true);
            };
            u32::try_from(first.start()).unwrap_or(0)
        };

        self.record(line_num, line_bytes, col);
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        // Context lines (`-A`/`-B`/`-C`) render in the same
        // `path:line#scope:<verbatim>` shape as match lines — each becomes a hit
        // and is anchored by containment at its own line. No match column.
        let Some(line_num) = ctx.line_number().and_then(|n| u32::try_from(n).ok()) else {
            return Ok(true);
        };
        self.record(line_num, ctx.bytes(), 0);
        Ok(true)
    }
}

// ─── Alternation splitting ────────────────────────────────────────────

/// One file the ripgrep walk skipped instead of searching.
///
/// Carries the absolute path, why it was skipped, and whether the path was
/// explicitly **named** (a file root — a positional arg or a glob that
/// expanded to it) versus reached by a directory walk. Folded into the
/// wire-ready [`GrepSkips`] by [`GrepSkips::from_records`] (misc 135, bug 62).
///
/// `pub` since ws43-02: the hitstream engine's CLI-side walk records the same
/// skips (its [`WalkSummary`](crate::hitstream::engine::WalkSummary) carries
/// them), so a skip is reported identically whichever walk found it.
#[derive(Debug)]
pub struct SkipRecord {
    /// Absolute path of the skipped file.
    pub path: PathBuf,
    /// Why the classifier treated it as unsearchable.
    pub reason: BinarySkip,
    /// The path was explicitly named (per-file reporting) vs walked (aggregated).
    pub named: bool,
}

/// Result of a ripgrep line search.
#[derive(Default)]
struct RipgrepMatches {
    /// Per-file line numbers.
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line `(full source line, first-match column)` — the atom
    /// text (one-atom model, decision 024) plus the first match's column for
    /// hit classification and `prepareRename` positioning.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
    /// Every regular file the walk visited, with its `(absolute path, mtime)`
    /// — not just the files that matched the pattern. Feeds the WS31 changed-set
    /// baseline diff (Consumer A): the manager filters these to the union of
    /// registered watch globs, diffs against the per-root baseline, and routes
    /// the delta per server. The stat is free here — the walk already reads each
    /// file (`grep_server.rs` ripgrep walk).
    files: Vec<(PathBuf, i64)>,
    /// Files skipped instead of searched (binary content — a NUL before any text
    /// BOM), so the skip is reported rather than read as a no-match (misc 135).
    skips: Vec<SkipRecord>,
}

impl RipgrepMatches {
    /// Merges per-thread match accumulators into a single result.
    fn merge(parts: Vec<ThreadMatches>) -> Self {
        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>> = HashMap::new();
        let mut files: Vec<(PathBuf, i64)> = Vec::new();
        let mut skips: Vec<SkipRecord> = Vec::new();

        for part in parts {
            for (file, lines) in part.file_lines {
                file_lines.entry(file).or_default().extend(lines);
            }
            for (file, line_map) in part.file_line_texts {
                let entry = file_line_texts.entry(file).or_default();
                for (line, texts) in line_map {
                    entry.entry(line).or_default().extend(texts);
                }
            }
            files.extend(part.files);
            skips.extend(part.skips);
        }

        Self {
            file_lines,
            file_line_texts,
            files,
            skips,
        }
    }
}

/// Unwraps the shared collector into the per-thread parts after the parallel
/// walk completes.
///
/// A walker thread that panicked poisons `collected`; recover the poison via
/// [`std::sync::PoisonError::into_inner`] — matching [`CollectOnDrop::drop`] —
/// so the matches its siblings already pushed survive instead of being lost to
/// a hard grep error. Errors only if a walker thread still holds a reference to
/// the `Arc`, which never happens once `walker.run` has returned.
///
/// Like [`CollectOnDrop`]'s recovery, this poison handling is a **test-profile
/// safety net**: poisoning requires a sibling *unwind* (`panic = "unwind"`). The
/// release profile is `panic = "abort"` (`Cargo.toml`), so a walker panic aborts
/// the daemon and this branch never runs in production; release correctness
/// relies on the (panic-free) walker closure.
///
/// # Errors
///
/// Returns an error if a walker thread still holds an `Arc` reference (the
/// `Arc::into_inner` returns `None`), which cannot occur after the walk joins.
fn harvest(collected: Arc<std::sync::Mutex<Vec<ThreadMatches>>>) -> Result<Vec<ThreadMatches>> {
    Ok(Arc::into_inner(collected)
        .ok_or_else(|| anyhow!("walker threads still hold references"))?
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

/// Per-thread match accumulator used during parallel file walking.
#[derive(Default)]
struct ThreadMatches {
    /// Per-file line numbers.
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line `(full source line, first-match column)`.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
    /// Every regular file this thread visited, `(absolute path, mtime)` — the
    /// WS31 changed-set baseline observation set.
    files: Vec<(PathBuf, i64)>,
    /// Files this thread skipped instead of searching (misc 135).
    skips: Vec<SkipRecord>,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

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

    /// Build a `GrepHit` with an explicit anchor and verbatim line.
    fn hit(file: &str, line: u32, text: &str, anchor: Anchor) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            matched_text: text.to_string(),
            anchor,
        }
    }

    // ─── Bounded shaping memory: shape_hits + spool (misc 140 phase 2) ─────

    /// Representative result shapes for the byte-identity contract: empty,
    /// single-file, multi-file with walk order ≠ sort order and mixed anchors,
    /// context-style multi-line, scope-trail enrichment, and trailing whitespace
    /// on the final line (which the render trims).
    fn shape_fixtures() -> Vec<Vec<GrepHit>> {
        vec![
            vec![],
            vec![hit("/project/src/a.rs", 9, "fn run() {", Anchor::TopLevel)],
            vec![
                hit("/project/src/b.rs", 4, "b4", Anchor::TopLevel),
                hit(
                    "/project/src/a.rs",
                    9,
                    "a9",
                    Anchor::Scope("Outer/inner".to_string()),
                ),
                hit("/project/src/a.rs", 2, "a2", Anchor::Unknown),
                hit("/project/src/c.rs", 1, "c1", Anchor::TopLevel),
            ],
            vec![
                hit(
                    "/project/x.rs",
                    10,
                    "let x = 1;",
                    Anchor::Scope("m".to_string()),
                ),
                hit(
                    "/project/x.rs",
                    11,
                    "let y = 2;",
                    Anchor::Scope("m".to_string()),
                ),
                hit(
                    "/project/x.rs",
                    12,
                    "let z = 3;",
                    Anchor::Scope("m".to_string()),
                ),
            ],
            vec![
                hit("/project/a.rs", 1, "first", Anchor::TopLevel),
                hit(
                    "/project/z.rs",
                    1,
                    "trailing spaces here   ",
                    Anchor::TopLevel,
                ),
            ],
        ]
    }

    #[test]
    fn shape_hits_byte_identical_to_render_across_threshold() {
        let fs = test_fs("/project");
        let cwd = Some(Path::new("/project"));
        for hits in shape_fixtures() {
            let oracle = render_results(&hits, &fs, cwd);

            // Everything in memory (the hot path): a huge threshold never spools.
            let hot = shape_hits(&hits, &fs, cwd, usize::MAX).expect("shape hot");
            assert!(hot.spool_path().is_none(), "hot path must not spool");
            assert_eq!(
                hot.materialize(),
                oracle,
                "in-memory shaping must match the single-string render",
            );

            // Everything spooled: a zero threshold forces disk for every hunk.
            let spooled = shape_hits(&hits, &fs, cwd, 0).expect("shape spooled");
            assert_eq!(
                spooled.materialize(),
                oracle,
                "spooled shaping must be byte-identical to the in-memory hot path",
            );

            // Mixed: a small threshold keeps the first hunk in memory, spills the
            // rest — the boundary the hot/cold split runs through.
            let mixed = shape_hits(&hits, &fs, cwd, 8).expect("shape mixed");
            assert_eq!(
                mixed.materialize(),
                oracle,
                "mixed in-memory/spooled shaping must match the render",
            );
        }
    }

    #[test]
    fn shape_hits_preserves_sort_order_when_spooled() {
        let fs = test_fs("/project");
        let cwd = Some(Path::new("/project"));
        // Insertion order deliberately unsorted (mimicking parallel-walk order).
        let hits = vec![
            hit("/project/z.rs", 1, "z", Anchor::TopLevel),
            hit("/project/a.rs", 2, "a2", Anchor::TopLevel),
            hit("/project/a.rs", 1, "a1", Anchor::TopLevel),
            hit("/project/m.rs", 1, "m", Anchor::TopLevel),
        ];
        let spooled = shape_hits(&hits, &fs, cwd, 0).expect("shape spooled");
        // Line numbers render 1-based (`hit.line + 1`); sort is by (file, line).
        assert_eq!(
            spooled.materialize(),
            "a.rs:2:a1\na.rs:3:a2\nm.rs:2:m\nz.rs:2:z",
            "spooled hunks emit in (file, line) sort order, not insertion/walk order",
        );
    }

    #[test]
    fn spool_created_under_cache_dir_and_removed_on_drop() {
        let fs = test_fs("/project");
        let cwd = Some(Path::new("/project"));
        let hits = vec![hit("/project/a.rs", 1, "spooled hit", Anchor::TopLevel)];
        let shaped = shape_hits(&hits, &fs, cwd, 0).expect("shape spooled");

        let path = shaped
            .spool_path()
            .expect("a zero-threshold result spools to disk");
        assert!(
            path.starts_with(crate::paths::cache_dir()),
            "spool lives under cache_dir (never runtime_dir/tmpfs): {path:?}",
        );
        assert!(
            path.exists(),
            "spool exists while the shaped output is alive"
        );

        drop(shaped);
        assert!(
            !path.exists(),
            "spool is unlinked on drop — completion AND cancellation both drop ShapedOutput",
        );
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

    // ─── line format ───────────────────────────────────────────────────────

    #[test]
    fn render_top_level_hit_has_no_anchor() {
        // Enriched but genuinely top-level → a pure ripgrep line, no `#`.
        let fs = test_fs("/project");
        let hits = [hit("/project/src/a.rs", 9, "fn run() {", Anchor::TopLevel)];
        assert_eq!(render_results(&hits, &fs, None), "src/a.rs:10:fn run() {");
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

    #[test]
    fn render_scope_anchor_shoved_between_line_and_delimiter() {
        // `#scope` carries no whitespace, sits against `path:line`, then the
        // ripgrep `:` delimiter, then the byte-verbatim line.
        let fs = test_fs("/project");
        let hits = [hit(
            "/project/src/lib.rs",
            58,
            "    foo()",
            Anchor::Scope("A/B/run".to_string()),
        )];
        assert_eq!(
            render_results(&hits, &fs, None),
            "src/lib.rs:59#A/B/run:    foo()"
        );
    }

    #[test]
    fn render_unknown_anchor_is_question_mark() {
        // Could-not-enrich → `#?`, a distinct marker (never misread as top-level).
        let fs = test_fs("/project");
        let hits = [hit("/project/x.rs", 4, "some text", Anchor::Unknown)];
        assert_eq!(render_results(&hits, &fs, None), "x.rs:5#?:some text");
    }

    #[test]
    fn render_strips_to_byte_exact_ripgrep() {
        // Removing the `#…` fragment (from `#` to the next `:`) yields
        // `path:line:text` — the superset contract, for every anchor variant.
        let fs = test_fs("/project");
        for anchor in [
            Anchor::Scope("Outer".to_string()),
            Anchor::Unknown,
            Anchor::TopLevel,
        ] {
            let line = render_results(&[hit("/project/a.rs", 0, "code", anchor)], &fs, None);
            let stripped = line.find('#').map_or_else(
                || line.clone(),
                |hash| {
                    let colon = line[hash..].find(':').expect("delimiter after fragment") + hash;
                    format!("{}{}", &line[..hash], &line[colon..])
                },
            );
            assert_eq!(stripped, "a.rs:1:code", "strip of {line:?}");
        }
    }

    #[test]
    fn render_orders_by_file_then_line() {
        let fs = test_fs("/project");
        let hits = [
            hit("/project/src/b.rs", 4, "b4", Anchor::TopLevel),
            hit("/project/src/a.rs", 9, "a9", Anchor::TopLevel),
            hit("/project/src/a.rs", 2, "a2", Anchor::Unknown),
        ];
        assert_eq!(
            render_results(&hits, &fs, None),
            "src/a.rs:3#?:a2\nsrc/a.rs:10:a9\nsrc/b.rs:5:b4"
        );
    }

    #[test]
    fn render_is_byte_stable_across_runs() {
        // `(file, line)` ordering → reproducible bytes (the misc-32 pattern).
        let fs = test_fs("/project");
        let hits = [
            hit("/project/src/b.rs", 4, "fn fn_b() {", Anchor::TopLevel),
            hit("/project/src/a.rs", 9, "fn fn_a() {", Anchor::TopLevel),
            hit("/project/src/a.rs", 2, "// fn_a usage", Anchor::TopLevel),
        ];
        assert_eq!(
            render_results(&hits, &fs, None),
            render_results(&hits, &fs, None)
        );
    }

    #[test]
    fn render_cwd_relative_no_header() {
        // cwd-scoped: cwd-relative path, NO `cwd:` header, NO root header.
        let fs = test_fs("/project");
        let hits = [hit(
            "/project/src/lib.rs",
            10,
            "struct MyStruct {",
            Anchor::Scope("mod_x".to_string()),
        )];
        let out = render_results(&hits, &fs, Some(Path::new("/project")));
        assert_eq!(out, "src/lib.rs:11#mod_x:struct MyStruct {");
        assert!(!out.contains("cwd:"), "no cwd header: {out}");
    }

    #[test]
    fn render_verbatim_preserves_indentation_with_no_padding() {
        // The raw floats — indentation intact, no padding added.
        let fs = test_fs("/project");
        let hits = [hit(
            "/project/a.rs",
            0,
            "        deeply_indented();",
            Anchor::TopLevel,
        )];
        assert_eq!(
            render_results(&hits, &fs, None),
            "a.rs:1:        deeply_indented();"
        );
    }

    #[test]
    fn name_embedding_server_heading_is_clean_source_line() {
        // A markdown heading hit: the verbatim raw is the clean source line,
        // with no kind label.
        let fs = test_fs("/project");
        let hits = [hit("/project/doc.md", 0, "# Title", Anchor::TopLevel)];
        let out = render_results(&hits, &fs, None);
        assert_eq!(out, "doc.md:1:# Title");
        assert!(!out.contains('<'), "no kind label: {out}");
    }

    // ─── CollectOnDrop ──────────────────────────────────────────────────

    #[test]
    fn collect_on_drop_pushes_non_empty() {
        let collected = Arc::new(std::sync::Mutex::new(Vec::<ThreadMatches>::new()));
        {
            let mut state = CollectOnDrop {
                local: ThreadMatches::default(),
                collected: Arc::clone(&collected),
            };
            state
                .local
                .file_lines
                .entry("test.rs".to_string())
                .or_default()
                .push(1);
        }
        let vec = collected.lock().expect("lock");
        assert_eq!(vec.len(), 1, "non-empty local should be pushed on drop");
        assert!(vec[0].file_lines.contains_key("test.rs"));
        drop(vec);
    }

    #[test]
    fn collect_on_drop_skips_empty() {
        let collected = Arc::new(std::sync::Mutex::new(Vec::<ThreadMatches>::new()));
        {
            let _state = CollectOnDrop {
                local: ThreadMatches::default(),
                collected: Arc::clone(&collected),
            };
        }
        let vec = collected.lock().expect("lock");
        assert!(vec.is_empty(), "empty local should not be pushed");
        drop(vec);
    }

    // ─── RipgrepMatches::merge ──────────────────────────────────────────

    #[test]
    fn merge_combines_thread_matches() {
        let mut t1 = ThreadMatches::default();
        t1.file_lines.entry("a.rs".to_string()).or_default().push(1);
        t1.file_line_texts
            .entry("a.rs".to_string())
            .or_default()
            .entry(1)
            .or_default()
            .push(("foo".to_string(), 0));

        let mut t2 = ThreadMatches::default();
        t2.file_lines.entry("a.rs".to_string()).or_default().push(5);
        t2.file_lines
            .entry("b.rs".to_string())
            .or_default()
            .push(10);

        let merged = RipgrepMatches::merge(vec![t1, t2]);

        let a_lines = &merged.file_lines["a.rs"];
        assert!(a_lines.contains(&1), "a.rs should have line 1");
        assert!(a_lines.contains(&5), "a.rs should have line 5");
        let b_lines = &merged.file_lines["b.rs"];
        assert!(b_lines.contains(&10), "b.rs should have line 10");
        let a_texts = &merged.file_line_texts["a.rs"][&1];
        assert_eq!(a_texts[0].0, "foo");
    }

    #[test]
    fn merge_empty_parts_returns_empty() {
        let merged = RipgrepMatches::merge(vec![]);
        assert!(merged.file_lines.is_empty());
        assert!(merged.file_line_texts.is_empty());
    }

    // ─── CollectOnDrop poison recovery ──────────────────────────────────

    /// A poisoned `collected` mutex must still receive a dropping thread's
    /// matches — recovering the lock instead of silently discarding them.
    #[test]
    fn collect_on_drop_recovers_poisoned_lock() {
        let collected = Arc::new(std::sync::Mutex::new(Vec::<ThreadMatches>::new()));

        // Poison the mutex: panic in another thread while holding the guard.
        // `expect` on a `None` panics (and `expect` is allowed in tests),
        // avoiding the denied bare `panic!` macro.
        let poisoner = Arc::clone(&collected);
        let handle = std::thread::spawn(move || {
            let _guard = poisoner.lock().expect("lock to poison");
            // A runtime-empty iterator yields `None`; `expect` panics on it
            // (clippy can't const-fold this into a bare `panic!`).
            let empty: Vec<()> = Vec::new();
            empty
                .into_iter()
                .next()
                .expect("intentional panic to poison the mutex");
        });
        assert!(
            handle.join().is_err(),
            "poisoning thread should have panicked"
        );
        assert!(
            collected.lock().is_err(),
            "mutex should be poisoned after the panic"
        );

        // A CollectOnDrop carrying matches, dropped against the poisoned mutex.
        let mut local = ThreadMatches::default();
        local
            .file_lines
            .entry("poisoned.rs".to_string())
            .or_default()
            .push(7);
        let state = CollectOnDrop {
            local,
            collected: Arc::clone(&collected),
        };
        drop(state);

        // The matches were recovered, not discarded.
        let (len, has_key) = {
            let recovered = collected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                recovered.len(),
                recovered[0].file_lines.contains_key("poisoned.rs"),
            )
        };
        assert_eq!(len, 1, "dropped thread's matches must survive");
        assert!(
            has_key,
            "recovered matches must include the dropped accumulator"
        );
    }

    /// The final harvest in `ripgrep_matches` must recover a poisoned
    /// `collected` mutex — matching `CollectOnDrop::drop` — instead of erroring
    /// out and discarding every collected match (WS31-review R6 N2). A walker
    /// thread that panicked (poisoning the lock) after its siblings already
    /// pushed matches must not cost the whole grep its partial results.
    #[test]
    fn ws31_review_r6_final_collection_recovers_poison() {
        let collected = Arc::new(std::sync::Mutex::new(Vec::<ThreadMatches>::new()));

        // A sibling thread's matches are already in the collector before the
        // poison.
        {
            let mut local = ThreadMatches::default();
            local
                .file_lines
                .entry("survivor.rs".to_string())
                .or_default()
                .push(11);
            collected.lock().expect("fresh mutex lock").push(local);
        }

        // Poison the mutex: panic in another thread while holding the guard.
        // (Same idiom as `collect_on_drop_recovers_poisoned_lock`.)
        let poisoner = Arc::clone(&collected);
        let handle = std::thread::spawn(move || {
            let _guard = poisoner.lock().expect("lock to poison");
            let empty: Vec<()> = Vec::new();
            empty
                .into_iter()
                .next()
                .expect("intentional panic to poison the mutex");
        });
        assert!(
            handle.join().is_err(),
            "poisoning thread should have panicked"
        );
        assert!(
            collected.lock().is_err(),
            "mutex should be poisoned after the panic"
        );

        // The harvest must recover the poison and return the pushed matches,
        // not fail.
        let parts = harvest(collected).expect("harvest must recover the poisoned lock");
        assert_eq!(parts.len(), 1, "collected matches must survive the poison");
        assert!(
            parts[0].file_lines.contains_key("survivor.rs"),
            "the surviving accumulator must be returned, not lost to an error"
        );
    }

    // ─── MatchSink::matched ─────────────────────────────────────────────

    #[test]
    fn match_sink_collects_hits_with_line_numbers() {
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(true)
            .build("Config")
            .expect("valid regex");
        let mut local = ThreadMatches::default();
        let content = b"let x = Config::new();\nother line\nConfig again\n";
        {
            let mut sink = MatchSink {
                matcher: &matcher,
                path: "test.rs",
                local: &mut local,
                invert: false,
            };
            Searcher::new()
                .search_slice(&matcher, content, &mut sink)
                .expect("search should succeed");
        }

        let lines = &local.file_lines["test.rs"];
        assert!(lines.contains(&1), "line 1 should match: {lines:?}");
        assert!(lines.contains(&3), "line 3 should match: {lines:?}");
        assert!(!lines.contains(&2), "line 2 should not match: {lines:?}");

        let texts = &local.file_line_texts["test.rs"];
        let line1_texts = &texts[&1];
        assert!(
            line1_texts.iter().any(|(t, _)| t.contains("Config")),
            "matched text should contain Config: {line1_texts:?}"
        );
    }

    #[test]
    fn match_sink_records_column_offset() {
        let matcher = RegexMatcherBuilder::new()
            .build("world")
            .expect("valid regex");
        let mut local = ThreadMatches::default();
        let content = b"hello world\n";
        {
            let mut sink = MatchSink {
                matcher: &matcher,
                path: "test.rs",
                local: &mut local,
                invert: false,
            };
            Searcher::new()
                .search_slice(&matcher, content, &mut sink)
                .expect("search ok");
        }
        let texts = &local.file_line_texts["test.rs"][&1];
        let (_, col) = &texts[0];
        assert_eq!(*col, 6, "column offset should be 6, got {col}");
    }

    #[test]
    fn match_sink_captures_real_match_after_zero_width() {
        // Pattern `b?` matches empty string at offset 0 (zero-width),
        // then "b" at offset 1 (real match). The zero-width advance
        // (`at = m.end() + 1`) must skip past offset 0 so the real
        // match at offset 1 is found.
        let matcher = RegexMatcherBuilder::new().build("b?").expect("valid regex");
        let mut local = ThreadMatches::default();
        let content = b"abc\n";
        {
            let mut sink = MatchSink {
                matcher: &matcher,
                path: "test.rs",
                local: &mut local,
                invert: false,
            };
            Searcher::new()
                .search_slice(&matcher, content, &mut sink)
                .expect("search ok");
        }
        let texts = &local.file_line_texts["test.rs"][&1];
        assert!(
            texts.iter().any(|(t, _)| t.contains('b')),
            "real match 'b' after zero-width should be captured: {texts:?}"
        );
    }

    // ─── named-path gitignore bypass (misc 110) ─────────────────────────

    fn git_init(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");
    }

    #[test]
    fn ripgrep_matches_named_gitignored_file_is_searched() {
        // A gitignored file named explicitly on the command line is searched
        // unconditionally — naming it is a direct request for that exact file,
        // so the gitignore gate does not apply even without
        // `--include-gitignored` (misc 110, ripgrep parity).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(root.join("ignored.rs"), "TODO ignored\n").expect("write");

        let file = root.join("ignored.rs");
        let fs = Arc::new(FilesystemManager::new());
        let rg = GrepServer::ripgrep_matches(
            "TODO",
            std::slice::from_ref(&file),
            &no_exclude(),
            false, // include_gitignored = false: the bypass must not depend on it
            false,
            &fs,
            &GrepFlags::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("ripgrep_matches");

        assert!(
            rg.file_lines.keys().any(|k| k.ends_with("ignored.rs")),
            "named gitignored file must be searched without --include-gitignored: {:?}",
            rg.file_lines
        );
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

    // ─── flag parity: searcher (-v invert / -A/-B/-C context) ───────────

    /// The empty exclude set — the no-`--exclude-pattern` case, wired as
    /// `ripgrep_matches` now takes it (an `Arc<ExcludeSet>`, not an `Option`).
    fn no_exclude() -> Arc<ExcludeSet> {
        Arc::new(ExcludeSet::default())
    }

    /// Run `ripgrep_matches` over a one-file tempdir and return the matched
    /// line numbers (sorted), the line→texts present.
    fn rg_over(dir: &Path, pattern: &str, flags: &GrepFlags) -> RipgrepMatches {
        let fs = Arc::new(FilesystemManager::new());
        GrepServer::ripgrep_matches(
            pattern,
            std::slice::from_ref(&dir.to_path_buf()),
            &no_exclude(),
            false,
            false,
            &fs,
            flags,
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("ripgrep_matches")
    }

    #[test]
    fn invert_match_selects_non_matching_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("f.txt"), "alpha\nbeta\ngamma\n").expect("write");
        let flags = GrepFlags {
            invert: true,
            ..GrepFlags::default()
        };
        let rg = rg_over(tmp.path(), "beta", &flags);
        let lines = rg.file_line_texts.values().next().expect("one file");
        // The non-matching lines (1, 3) are selected; the matching line (2) is not.
        assert!(lines.contains_key(&1), "alpha selected: {lines:?}");
        assert!(lines.contains_key(&3), "gamma selected: {lines:?}");
        assert!(!lines.contains_key(&2), "beta excluded: {lines:?}");
    }

    #[test]
    fn context_captures_surrounding_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("f.txt"), "l1\nl2 HIT\nl3\nl4\n").expect("write");
        let flags = GrepFlags {
            before_context: 1,
            after_context: 1,
            ..GrepFlags::default()
        };
        let rg = rg_over(tmp.path(), "HIT", &flags);
        let lines = rg.file_line_texts.values().next().expect("one file");
        // The match (line 2) plus one line of context on each side (1, 3).
        assert!(lines.contains_key(&1), "before-context: {lines:?}");
        assert!(lines.contains_key(&2), "match: {lines:?}");
        assert!(lines.contains_key(&3), "after-context: {lines:?}");
        assert!(!lines.contains_key(&4), "no extra: {lines:?}");
    }

    // ─── flag parity: file filters (-g glob / --type) ───────────────────

    #[test]
    fn glob_filter_restricts_to_matching_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.rs"), "TODO here\n").expect("write");
        std::fs::write(tmp.path().join("b.txt"), "TODO here\n").expect("write");
        let flags = GrepFlags {
            globs: vec!["*.rs".to_string()],
            ..GrepFlags::default()
        };
        let rg = rg_over(tmp.path(), "TODO", &flags);
        assert!(
            rg.file_lines.keys().any(|k| k.ends_with("a.rs")),
            "the .rs file is searched: {:?}",
            rg.file_lines
        );
        assert!(
            !rg.file_lines.keys().any(|k| k.ends_with("b.txt")),
            "the .txt file is filtered out by -g '*.rs': {:?}",
            rg.file_lines
        );
    }

    #[test]
    fn type_filter_restricts_to_matching_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.rs"), "TODO here\n").expect("write");
        std::fs::write(tmp.path().join("b.md"), "TODO here\n").expect("write");
        let flags = GrepFlags {
            types: vec!["rust".to_string()],
            ..GrepFlags::default()
        };
        let rg = rg_over(tmp.path(), "TODO", &flags);
        assert!(
            rg.file_lines.keys().any(|k| k.ends_with("a.rs")),
            "the rust file is searched: {:?}",
            rg.file_lines
        );
        assert!(
            !rg.file_lines.keys().any(|k| k.ends_with("b.md")),
            "the md file is filtered out by --type rust: {:?}",
            rg.file_lines
        );
    }

    // ─── empty pattern: ripgrep parity (bug 83) ────────────────────────

    impl GrepOutcome {
        /// The `(matches, files)` totals of a `--count` outcome, else `None` —
        /// the `Option`-accessor idiom this module uses to avoid a denied bare
        /// `panic!` on the wrong variant.
        const fn count(&self) -> Option<(usize, usize)> {
            if let Self::Count { matches, files, .. } = self {
                Some((*matches, *files))
            } else {
                None
            }
        }

        /// The reconstructed rendered string of a `Rendered` outcome, else `None`.
        fn rendered(&self) -> Option<String> {
            if let Self::Rendered { output, .. } = self {
                Some(output.materialize())
            } else {
                None
            }
        }
    }

    /// Build a daemon-less [`GrepServer`] (LSP-manager-empty, no roots) and run
    /// `execute` over `params`. The exact server the daemon serves, so this
    /// drives the real `execute` path where the empty-pattern short-circuit was
    /// removed (bug 83).
    fn execute_daemon_less(params: &serde_json::Value) -> GrepOutcome {
        let search = crate::bridge::DaemonlessSearch::from_config(crate::config::Config::default());
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(search.grep.execute(params, None, &cancel))
            .expect("grep execute")
    }

    /// Bug 83: `catenary grep '' --count` has ripgrep parity — the empty pattern
    /// matches every line, so the count is the file's line count (not the silent
    /// zero the removed short-circuit produced).
    #[test]
    fn empty_pattern_count_equals_line_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A fixture with a known line count: five lines, each `\n`-terminated.
        let fixture = tmp.path().join("fixture.txt");
        std::fs::write(&fixture, "alpha\nbeta\ngamma\ndelta\nepsilon\n").expect("write fixture");

        let outcome = execute_daemon_less(&serde_json::json!({
            "pattern": "",
            "paths": [fixture.to_string_lossy()],
            "count": true,
        }));

        let (matches, files) = outcome.count().expect("count query yields a Count outcome");
        assert_eq!(matches, 5, "empty pattern counts every line");
        assert_eq!(files, 1, "the single fixture file matched");
    }

    /// Bug 83: without `--count`, the empty pattern returns every line (enrichment
    /// riding along where covered is fine — asserted here is only that no line is
    /// dropped, so every fixture line appears in the rendered output).
    #[test]
    fn empty_pattern_returns_every_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fixture = tmp.path().join("fixture.txt");
        let lines = ["alpha", "beta", "gamma", "delta", "epsilon"];
        std::fs::write(&fixture, format!("{}\n", lines.join("\n"))).expect("write fixture");

        let outcome = execute_daemon_less(&serde_json::json!({
            "pattern": "",
            "paths": [fixture.to_string_lossy()],
        }));

        let rendered = outcome
            .rendered()
            .expect("non-count query yields a Rendered outcome");
        for line in lines {
            assert!(
                rendered.contains(line),
                "empty pattern must return every line; `{line}` missing from:\n{rendered}"
            );
        }
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

    #[test]
    fn ripgrep_matches_dir_walk_still_gates_gitignored_contents() {
        // The file-bypass must NOT leak into directory walks: a gitignored file
        // reached by walking a named DIRECTORY root is still skipped — the gate
        // governs the recursive walk, where `--include-gitignored` remains the
        // opt-in (directory-walk behavior unchanged, misc 110). The walk starts
        // at the repo root so the `.gitignore` rule that excludes `target/` is
        // in scope for the descent (an `ignore` walk only consults ignore files
        // at or below its start path).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        git_init(&root);
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
        std::fs::write(root.join("kept.rs"), "TODO kept\n").expect("write");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::write(root.join("target/ignored.rs"), "TODO buried\n").expect("write");

        let fs = Arc::new(FilesystemManager::new());

        // Gated walk: the non-ignored file is found, the gitignored one under
        // `target/` is skipped.
        let gated = GrepServer::ripgrep_matches(
            "TODO",
            std::slice::from_ref(&root),
            &no_exclude(),
            false,
            false,
            &fs,
            &GrepFlags::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("ripgrep_matches");
        assert!(
            gated.file_lines.keys().any(|k| k.ends_with("kept.rs")),
            "the non-ignored file is found in the walk: {:?}",
            gated.file_lines
        );
        assert!(
            !gated.file_lines.keys().any(|k| k.ends_with("ignored.rs")),
            "gitignored contents must be skipped in a directory walk without \
             --include-gitignored: {:?}",
            gated.file_lines
        );

        // The escape hatch lifts the directory-walk gate.
        let with_ignored = GrepServer::ripgrep_matches(
            "TODO",
            std::slice::from_ref(&root),
            &no_exclude(),
            true,
            false,
            &fs,
            &GrepFlags::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("ripgrep_matches");
        assert!(
            with_ignored
                .file_lines
                .keys()
                .any(|k| k.ends_with("ignored.rs")),
            "--include-gitignored surfaces the ignored dir's contents: {:?}",
            with_ignored.file_lines
        );
    }

    // ─── content classification & cancellation (misc 140) ──────────────────

    /// bug 62: a pure-UTF-8 file well over the retired 10 MB cap is searched to
    /// EOF and matched from both a named path and a directory walk — never
    /// skipped-by-size (the cap that misclassified such files is gone).
    #[test]
    fn ripgrep_matches_searches_large_utf8_file_uncapped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let big = root.join("bundle.js");
        // 29 bytes/line * 400_000 ≈ 11.6 MB, every line holds the needle.
        std::fs::write(&big, "NEEDLE and some padding text\n".repeat(400_000)).expect("write");
        assert!(
            std::fs::metadata(&big).expect("meta").len() > 10 * 1024 * 1024,
            "fixture must exceed the retired cap"
        );
        let fs = Arc::new(FilesystemManager::new());
        let token = tokio_util::sync::CancellationToken::new();

        let named = GrepServer::ripgrep_matches(
            "NEEDLE",
            std::slice::from_ref(&big),
            &no_exclude(),
            false,
            false,
            &fs,
            &GrepFlags::default(),
            &token,
        )
        .expect("ripgrep_matches named");
        assert!(
            named.skips.is_empty(),
            "a large pure-UTF-8 file is not a skip: {:?}",
            named.skips
        );
        assert!(
            named.file_lines.keys().any(|k| k.ends_with("bundle.js")),
            "the named large file matched: {:?}",
            named.file_lines
        );

        let walked = GrepServer::ripgrep_matches(
            "NEEDLE",
            std::slice::from_ref(&root.to_path_buf()),
            &no_exclude(),
            false,
            false,
            &fs,
            &GrepFlags::default(),
            &token,
        )
        .expect("ripgrep_matches walk");
        assert!(walked.skips.is_empty(), "walk: large UTF-8 is not a skip");
        assert!(
            walked.file_lines.keys().any(|k| k.ends_with("bundle.js")),
            "the walked large file matched: {:?}",
            walked.file_lines
        );
    }

    /// A file with an early NUL (no BOM) is skipped as binary from both a named
    /// path (per-file skip) and a directory walk (aggregated skip) — the only
    /// skips left are content skips (misc 140).
    #[test]
    fn ripgrep_matches_skips_early_nul_file_named_and_walked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let blob = root.join("blob.dat");
        let mut bytes = b"NEEDLE".to_vec();
        bytes.push(0x00); // NUL before any BOM ⇒ binary
        bytes.extend_from_slice(b"more NEEDLE bytes");
        std::fs::write(&blob, &bytes).expect("write");
        let fs = Arc::new(FilesystemManager::new());
        let token = tokio_util::sync::CancellationToken::new();

        let named = GrepServer::ripgrep_matches(
            "NEEDLE",
            std::slice::from_ref(&blob),
            &no_exclude(),
            false,
            false,
            &fs,
            &GrepFlags::default(),
            &token,
        )
        .expect("named");
        assert!(
            named.file_lines.is_empty(),
            "a binary file yields no matches"
        );
        assert_eq!(named.skips.len(), 1, "one skip recorded: {:?}", named.skips);
        assert!(named.skips[0].named, "a named binary file skips per-file");
        assert_eq!(named.skips[0].reason, BinarySkip::Binary);

        let walked = GrepServer::ripgrep_matches(
            "NEEDLE",
            std::slice::from_ref(&root.to_path_buf()),
            &no_exclude(),
            false,
            false,
            &fs,
            &GrepFlags::default(),
            &token,
        )
        .expect("walked");
        assert!(walked.file_lines.is_empty());
        assert!(
            walked
                .skips
                .iter()
                .any(|s| !s.named && s.reason == BinarySkip::Binary),
            "a walked binary file is an unnamed skip: {:?}",
            walked.skips
        );
    }

    /// A fired cancel token quits the parallel walk (`WalkState::Quit`) before it
    /// records any file — the visit count collapses from the full tree to zero
    /// (misc 140 real walk cancellation). The un-fired baseline visits every file.
    #[test]
    fn ripgrep_matches_quits_when_token_fires() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let total = 400;
        for i in 0..total {
            std::fs::write(root.join(format!("f{i}.txt")), "needle\n").expect("write");
        }
        let fs = Arc::new(FilesystemManager::new());

        // Baseline: an un-fired token visits every file.
        let fresh = tokio_util::sync::CancellationToken::new();
        let full = GrepServer::ripgrep_matches(
            "needle",
            std::slice::from_ref(&root.to_path_buf()),
            &no_exclude(),
            false,
            false,
            &fs,
            &GrepFlags::default(),
            &fresh,
        )
        .expect("full walk");
        assert_eq!(
            full.files.len(),
            total,
            "the un-cancelled walk visits every file"
        );

        // A pre-fired token quits at the first entry each thread reaches; the
        // cancel check precedes the file-record push, so the visit set is empty.
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        let quit = GrepServer::ripgrep_matches(
            "needle",
            std::slice::from_ref(&root.to_path_buf()),
            &no_exclude(),
            false,
            false,
            &fs,
            &GrepFlags::default(),
            &cancelled,
        )
        .expect("cancelled walk");
        assert!(
            quit.files.is_empty(),
            "a cancelled walk visits no files, got {}",
            quit.files.len()
        );
        assert!(
            quit.file_lines.is_empty(),
            "a cancelled walk matches nothing"
        );
    }
}
