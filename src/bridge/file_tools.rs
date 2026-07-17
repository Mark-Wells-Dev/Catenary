// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The glob pipeline: the structural read.
//!
//! glob is grep's counterpart: where grep enriches a *hit* by where it lives,
//! glob enriches a *file* by what's in it. Each resolved path is dispatched by
//! type:
//! - File path → header `path  (N lines)` + the file's `documentSymbol`
//!   outline, one node per line, re-indented by tree depth.
//! - Directory path → its immediate entries (one level, not recursive): files
//!   get their outline; subdirectories get `name/  (N files, M dirs)` — the
//!   immediate child counts that track the active flags, a preview of the next
//!   glob.
//!
//! ## ws43-03: the CLI owns the walk
//!
//! Since the glob cutover this module is the **CLI-side** pipeline: `catenary
//! glob` expands its pattern gitignore-aware in-process
//! ([`build_glob_plan`]), streams the plan's file set to the daemon annotator
//! over `tool/hitstream` batches, and renders the listing itself
//! ([`render_glob_plan`]) with whatever enrichment came back. The daemon-side
//! query executor (`GlobServer`, the `tool/glob` arm, the daemon-less twin)
//! retired with the cutover; the daemon's only glob surface is the outline
//! leg of the hitstream annotator
//! ([`HitstreamEnricher`](super::hitstream_enricher::HitstreamEnricher)),
//! which runs the outline render helpers kept here. Every dependency failure
//! degrades to "less enrichment," never "no paths."
//!
//! ## The ruled enrichment weight (ws43-03)
//!
//! **Listing shapes get top-level structure by default.** The finding: 45 to
//! 360KB of full symbol outlines for plain directory listings, reproduced five
//! times by five different agents. A listing shape (a matched directory, or
//! more than one matched file) requests [`EnrichmentWeight::Listing`] — each
//! file's top-level symbols only, no nested tree. `--outline` opts up to the
//! full tree on demand; the single-file outline shape (`catenary glob
//! src/main.rs`) keeps the full tree as its default. There is deliberately
//! **no enrichment-off flag** (`--paths` was rejected by ruling: a first-class
//! off-switch becomes the taught habit and the product dies by opt-out;
//! `--count` covers tallies, and a pipeline needing bare paths strips indented
//! enrichment downstream).
//!
//! A file whose language has no server, or whose `documentSymbol` fails, is
//! listed with a `no outline` marker rather than silently outline-less — the
//! same marker every degrade arm (daemon absent, old daemon, stream fault,
//! deadline) renders, so the listing bytes are identical across the matrix.
//! The kind is implicit in each declaration source line — no `SymbolKind` ever
//! surfaces.
//!
//! The output is always complete (decision 025): every path always lists, with
//! no volume branch — budgets bound enrichment only, and the host caps only
//! the final read at the end of a pipeline.

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use super::SourceLines;
use super::filesystem_manager::{
    FilesystemManager, STAT_RETRY_ATTEMPTS, format_file_size, mtime_nanos,
};
use super::session::{ArgResolution, ExcludeSet, expand_glob_patterns_grouped_cancellable};
use crate::hitstream::AnnotatedHit;
use crate::symbol_index::Symbol;

/// Test-only per-stage operation counters for the glob expansion path.
///
/// The bug-78 pathology tier (misc 159) asserts on **operation counts, not wall
/// clocks** (the bug-59 ruling): expansion walks ≤ c·N entries and issues ≤ c·N
/// stats regardless of how many siblings match, and `--count` reads zero file
/// content bytes. These counters make those work-based facts directly
/// observable. Content bytes are tallied in `filesystem_manager::scan_file`
/// ([`crate::bridge::filesystem_manager::scan_bytes`]).
#[cfg(test)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "sibling module session.rs reads these counters through the private mod"
)]
pub(crate) mod probe {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Entries visited by the expansion walk (`ResolvedGlob::expand_cancellable`).
    pub(crate) static EXPAND_ENTRIES: AtomicUsize = AtomicUsize::new(0);
    /// Entries visited by a per-resolved-directory enumeration
    /// (`collect_dir_entries`).
    pub(crate) static COLLECT_ENTRIES: AtomicUsize = AtomicUsize::new(0);
    /// Calls to `file_info` — each reads a file's content for its line count.
    pub(crate) static FILE_INFO_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Zeroes every counter (and the content-byte tally) before a measured run.
    pub(crate) fn reset() {
        EXPAND_ENTRIES.store(0, Ordering::Relaxed);
        COLLECT_ENTRIES.store(0, Ordering::Relaxed);
        FILE_INFO_CALLS.store(0, Ordering::Relaxed);
        crate::bridge::filesystem_manager::scan_bytes_reset();
    }
    /// Entries visited by the expansion walk since the last [`reset`].
    pub(crate) fn expand_entries() -> usize {
        EXPAND_ENTRIES.load(Ordering::Relaxed)
    }
    /// Per-resolved-directory entries visited since the last [`reset`].
    pub(crate) fn collect_entries() -> usize {
        COLLECT_ENTRIES.load(Ordering::Relaxed)
    }
    /// `file_info` calls since the last [`reset`].
    pub(crate) fn file_info_calls() -> usize {
        FILE_INFO_CALLS.load(Ordering::Relaxed)
    }
}

/// A filesystem entry collected during the glob directory pipeline.
#[allow(
    clippy::struct_excessive_bools,
    reason = "flags are independent boolean properties"
)]
struct GlobEntry {
    /// Display name (relative to listing root).
    name: String,
    /// Absolute path for tree-sitter queries.
    abs_path: PathBuf,
    /// True if this is a directory entry.
    is_dir: bool,
    /// Line count for text files (None for dirs and binaries).
    line_count: Option<usize>,
    /// Formatted size for binary files.
    binary_size: Option<String>,
    /// True if this is a symlink.
    is_symlink: bool,
    /// Symlink target path (for display).
    symlink_target: Option<String>,
    /// True if this is a broken symlink (target missing).
    is_broken_symlink: bool,
    /// True if this entry is gitignored (only set when `include_gitignored`).
    is_gitignored: bool,
    /// True if this is a `.catenary_snapshot_*` sidecar file.
    is_snapshot: bool,
}

// ─── The CLI-side glob plan (ws43-03) ─────────────────────────────────

/// One file's enrichment as it came back over the annotation stream — the
/// CLI-side reading of an [`AnnotatedHit`]'s glob fields.
///
/// Built from the annotation batches via [`FileEnrichment::from_annotations`];
/// a file missing from the map (daemon absent, old daemon, stream fault,
/// deadline, or a pass-through batch) renders exactly as `covered: false` —
/// the `no outline` marker, the same spelling an uncovered file always
/// carried. Degrade-only: enrichment quality varies, the listing never does.
pub struct FileEnrichment {
    /// Whether enrichment covered the file (the wire `enriched` flag). `false`
    /// renders the `no outline` marker on a text file.
    pub covered: bool,
    /// The file has symbols but its outline is config-suppressed — renders the
    /// `[symbols available]` flag in place of the body.
    pub suppressed: bool,
    /// The rendered outline body (no base indent; the renderer re-indents each
    /// line for its listing position). `None` for a covered file with no
    /// symbols.
    pub outline: Option<String>,
}

impl FileEnrichment {
    /// Folds annotation-stream hits into the per-file enrichment map the
    /// renderer consumes. Later duplicates win (harmless — the daemon answers
    /// one hit per file).
    #[must_use]
    pub fn from_annotations(hits: Vec<AnnotatedHit>) -> HashMap<PathBuf, Self> {
        hits.into_iter()
            .map(|h| {
                (
                    h.hit.path,
                    Self {
                        covered: h.enriched,
                        suppressed: h.suppressed,
                        outline: h.outline,
                    },
                )
            })
            .collect()
    }
}

/// One node of the built listing: pre-rendered text (headers, subdirectory
/// entries, broken-symlink/snapshot lines), or a file whose header line and
/// outline body depend on enrichment.
enum PlanNode {
    /// A complete pre-rendered chunk (may span several lines, each
    /// newline-terminated).
    Text(String),
    /// A file rendered at enrichment time.
    File(FileNode),
}

/// A file entry whose final render depends on the enrichment map: the header
/// line's descriptor (`no outline`) and flags (`[symbols available]`,
/// `gitignored`), plus the outline body beneath it.
struct FileNode {
    /// The collected entry (name, counts, symlink/snapshot classification).
    entry: GlobEntry,
    /// Indent of the entry's header line: `""` for a directly-matched file,
    /// `"\t"` for a directory-listing entry.
    entry_indent: &'static str,
    /// Indent prepended to every outline body line: one level deeper than the
    /// header.
    outline_indent: &'static str,
}

/// The built glob listing, pre-enrichment.
///
/// Every path is already resolved and ordered (complete output, decision 025
/// — budgets bound enrichment only), alongside the file set that wants
/// outlines and the teaching signals the CLI turns into stderr notes (VERBS
/// teaching moments 2–4). Built CLI-side by [`build_glob_plan`]; rendered by
/// [`render_glob_plan`] once the annotation exchange (or its degrade arm)
/// resolves.
pub struct GlobPlan {
    /// The listing's nodes, in render order.
    nodes: Vec<PlanNode>,
    /// The pattern expanded to zero matches — the CLI renders the loud
    /// `no matches for pattern` report (misc 118) against the original
    /// argument spelling.
    pub no_match: bool,
    /// Display paths of matched directories (teaching moment 4): the CLI emits
    /// a `for its listing: catenary glob '<dir>/*'` hint per directory on
    /// stderr. Deduplicated, in first-seen order.
    pub dir_hints: Vec<String>,
    /// Result basenames carrying a glob metacharacter (teaching moment 3): the
    /// CLI teaches the escaped `'\*.md'` spelling on stderr; the result paths
    /// themselves stay byte-exact (ws35). Deduplicated, in first-seen order.
    pub metachar_names: Vec<String>,
    /// The files whose outlines the annotation exchange requests, in listing
    /// order, deduplicated. Everything the listing shows except directories,
    /// broken symlinks, and snapshot sidecars — the retired executor's
    /// enrich-always set.
    pub enrich_files: Vec<PathBuf>,
    /// The scoped-nudge observations (WS31 ticket 04): each resolved file, and
    /// each resolved directory's immediate entries, with walk-time mtimes.
    /// Shipped on the first annotation batch so the daemon's changed-set nudge
    /// lands before any outline is derived (the executor's nudge-then-anchor
    /// order). Add/update only — a scoped walk never reaps.
    pub observations: Vec<(PathBuf, i64)>,
    /// Whether the query resolved a listing shape (a matched directory, or
    /// more than one matched file) — the ruled listing-weight default. A
    /// single matched file is the file-outline shape and keeps the full tree.
    pub listing_shape: bool,
}

/// Builds the glob plan for one pattern (the arity-1 positional, absolute
/// and base-canonicalized).
///
/// Expands the pattern gitignore-aware, applies the exclude set, and lays out
/// the complete listing with its enrichment file set.
/// This is the retired daemon executor's dispatch loop
/// (`handle_literal_paths`), run CLI-side: a matched directory contributes its
/// header and one-level entries; a matched file contributes its header line.
/// The scope-level `(no LSP)` label retired with the cutover — the CLI cannot
/// see the daemon's mounted roots, and degradation is per-file (`no outline`),
/// mirroring grep's ws43-02 retirement of its `(no LSP)`/`cwd:` headers for
/// the per-line `#?` marker.
///
/// # Errors
///
/// Returns an error if a matched directory disappears mid-walk (its
/// canonicalization or enumeration fails) — the retired executor's same
/// usage-error class.
pub fn build_glob_plan(
    fs_manager: &FilesystemManager,
    pattern: &Path,
    exclude: &ExcludeSet,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<GlobPlan> {
    let cancel = CancellationToken::new();
    let mut groups = expand_glob_patterns_grouped_cancellable(
        std::slice::from_ref(&pattern.to_path_buf()),
        include_gitignored,
        include_hidden,
        &cancel,
    );
    // Apply the compiled exclude to the pattern's matches (bug 73): filtering
    // here — before the flat set feeds the scoped observations and the layout
    // loop — keeps the listing, the nudge, and `--count` (which filters
    // identically) in agreement, and leaves a fully-excluded pattern with an
    // empty set, so it reports the honest `no matches for pattern` rather than
    // vanishing.
    apply_exclude_to_groups(fs_manager, &mut groups, exclude);
    let resolved: Vec<PathBuf> = groups
        .iter()
        .flat_map(|g| g.resolved.iter().cloned())
        .collect();

    // The scoped-nudge observations (WS31 ticket 04), gathered under the same
    // visibility and exclude filters the listing applies. They ride the first
    // annotation batch.
    let observations =
        collect_scoped_observations(&resolved, include_gitignored, include_hidden, exclude);

    // The ruled weight shape: exactly one matched path, and it is a file →
    // the file-outline shape (full tree by default). Anything else — a
    // matched directory, several files, or nothing — is a listing.
    let listing_shape = resolved.len() != 1 || resolved[0].is_dir();

    let mut plan = GlobPlan {
        nodes: Vec::new(),
        no_match: resolved.is_empty(),
        dir_hints: Vec::new(),
        metachar_names: Vec::new(),
        enrich_files: Vec::new(),
        observations,
        listing_shape,
    };
    let mut enrich_seen: HashSet<PathBuf> = HashSet::new();

    for path in &resolved {
        // Teaching moment 3: a matched name that carries a glob metacharacter
        // is reachable only by the escaped spelling (`'\*.md'`). Record its
        // basename so the CLI teaches it; the result path stays byte-exact.
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            && name.contains(['*', '?', '[', ']', '{', '}'])
            && !plan.metachar_names.contains(&name)
        {
            plan.metachar_names.push(name);
        }
        // Directories first — `is_dir()` follows symlinks, so a symlink-to-dir
        // lists its contents (rather than rendering as a single file header).
        // This dir-first order matches `collect_scoped_observations` so the
        // listing and the changed-set nudge classify a symlink-to-dir the same
        // way (WS31-review walk-2).
        if path.is_dir() {
            // Teaching moment 4: the pattern resolved a directory; record its
            // display path so the CLI hands over the listing spelling
            // (`catenary glob '<dir>/*'`).
            let hint = path.to_string_lossy().into_owned();
            if !plan.dir_hints.contains(&hint) {
                plan.dir_hints.push(hint);
            }
            plan_dir(
                &mut plan,
                &mut enrich_seen,
                fs_manager,
                path,
                exclude,
                include_gitignored,
                include_hidden,
            )?;
        } else if path_is_file_or_symlink_with_retry(path) {
            // Re-stat with a bounded retry: a transient
            // `is_file()`/`is_symlink()` miss (an atomic-rename write racing
            // this fresh stat) must not silently skip a matched file that the
            // pattern expansion already confirmed present on disk.
            plan_file(&mut plan, &mut enrich_seen, fs_manager, path);
        }
        // Skip non-existent paths silently — the expansion shouldn't produce
        // them, but be defensive.
    }
    Ok(plan)
}

/// Lays out one matched directory: its `dir/  (N files, M dirs)` header, its
/// sorted subdirectory entries (child counts, no recursion), then its sorted
/// file entries as enrichment-dependent [`FileNode`]s — the retired
/// `handle_glob_dir` + `render_dir` shape, minus the index reads (enrichment
/// now arrives over the annotation stream).
fn plan_dir(
    plan: &mut GlobPlan,
    enrich_seen: &mut HashSet<PathBuf>,
    fs_manager: &FilesystemManager,
    dir: &Path,
    exclude: &ExcludeSet,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<()> {
    let canonical = dir
        .canonicalize()
        .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

    let entries = collect_dir_entries(
        fs_manager,
        &canonical,
        include_gitignored,
        include_hidden,
        exclude,
        &CancellationToken::new(),
    )?;

    // The target's own count = its immediate entries (what this glob
    // enumerated), split into files and directories — the same split a subdir
    // child entry shows, so the two forms agree.
    let target_files = entries.iter().filter(|e| !e.is_dir).count();
    let target_dirs = entries.iter().filter(|e| e.is_dir).count();
    let mut header = String::new();
    let _ = writeln!(
        header,
        "{}/  {}",
        canonical.display(),
        dir_count_suffix(target_files, target_dirs)
    );
    plan.nodes.push(PlanNode::Text(header));

    if entries.is_empty() {
        return Ok(());
    }

    let indent = "\t";
    let (dirs, files): (Vec<GlobEntry>, Vec<GlobEntry>) =
        entries.into_iter().partition(|e| e.is_dir);

    // Subdirectories first (sorted), each with its immediate child counts.
    let mut dirs = dirs;
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut dir_text = String::new();
    for d in &dirs {
        let (files_n, subdirs_n) =
            count_dir_children(&d.abs_path, include_gitignored, include_hidden, exclude);
        let descriptor = dir_count_suffix(files_n, subdirs_n);
        let flags = compute_entry_flags(false, false, d.is_gitignored);
        render_entry_line(&mut dir_text, d, &descriptor, &flags, indent);
    }
    if !dir_text.is_empty() {
        plan.nodes.push(PlanNode::Text(dir_text));
    }

    // Files (sorted), each an enrichment-dependent node (enrich always).
    let mut files = files;
    files.sort_by(|a, b| a.name.cmp(&b.name));
    for f in files {
        if f.is_broken_symlink || f.is_snapshot {
            let mut line = String::new();
            let flags = compute_entry_flags(false, false, f.is_gitignored);
            render_entry_line(&mut line, &f, "", &flags, indent);
            plan.nodes.push(PlanNode::Text(line));
            continue;
        }
        if enrich_seen.insert(f.abs_path.clone()) {
            plan.enrich_files.push(f.abs_path.clone());
        }
        plan.nodes.push(PlanNode::File(FileNode {
            entry: f,
            entry_indent: indent,
            outline_indent: "\t\t",
        }));
    }
    Ok(())
}

/// Lays out one directly-matched file: its `path  (N lines)` header (absolute
/// display) as an enrichment-dependent [`FileNode`] — the retired
/// `handle_glob_file` shape. A snapshot sidecar renders its dedicated
/// `[snapshot]` form and requests no enrichment.
fn plan_file(
    plan: &mut GlobPlan,
    enrich_seen: &mut HashSet<PathBuf>,
    fs_manager: &FilesystemManager,
    path: &Path,
) {
    let display = path.to_string_lossy().into_owned();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let metadata = std::fs::metadata(path).ok();

    if is_snapshot(&name) {
        let mut line = String::new();
        let _ = writeln!(line, "{display} [snapshot]");
        plan.nodes.push(PlanNode::Text(line));
        return;
    }

    let line_count = metadata
        .as_ref()
        .and_then(|m| fs_manager.line_count(path, m));
    // Header parity with the retired executor: a text file carries its line
    // count; anything else (binary, or a broken symlink whose metadata read
    // failed) carries its byte size.
    let binary_size = if line_count.is_none() {
        Some(format_file_size(metadata.map_or(0, |m| m.len())))
    } else {
        None
    };

    if enrich_seen.insert(path.to_path_buf()) {
        plan.enrich_files.push(path.to_path_buf());
    }
    plan.nodes.push(PlanNode::File(FileNode {
        entry: GlobEntry {
            name: display,
            abs_path: path.to_path_buf(),
            is_dir: false,
            line_count,
            binary_size,
            // A directly-matched symlink renders by its named spelling (no
            // `-> target` arrow) — the retired `handle_glob_file` shape.
            is_symlink: false,
            symlink_target: None,
            is_broken_symlink: false,
            is_gitignored: false,
            is_snapshot: false,
        },
        entry_indent: "",
        outline_indent: "\t",
    }));
}

/// Renders the built plan with the enrichment that actually arrived.
///
/// Every path in the plan always renders (decision 025 — complete output);
/// the map only decides each file's descriptor (`no outline`), flags
/// (`[symbols available]`, `gitignored`), and outline body. An empty map — the
/// degrade matrix's every arm — renders the complete listing, unenriched, with
/// the same bytes daemon-absent always produced.
#[must_use]
#[allow(
    clippy::implicit_hasher,
    reason = "the map is always the std-hasher one FileEnrichment::from_annotations builds"
)]
pub fn render_glob_plan(plan: &GlobPlan, enrichment: &HashMap<PathBuf, FileEnrichment>) -> String {
    let mut out = String::new();
    for node in &plan.nodes {
        match node {
            PlanNode::Text(text) => out.push_str(text),
            PlanNode::File(node) => {
                render_file_node(&mut out, node, enrichment.get(&node.entry.abs_path));
            }
        }
    }
    out
}

/// Renders one enrichment-dependent file node: the header line (descriptor +
/// flags), then the re-indented outline body when one arrived.
fn render_file_node(out: &mut String, node: &FileNode, enrichment: Option<&FileEnrichment>) {
    let covered = enrichment.is_some_and(|e| e.covered);
    let suppressed = covered && enrichment.is_some_and(|e| e.suppressed);
    let outline = if covered && !suppressed {
        enrichment.and_then(|e| e.outline.as_deref())
    } else {
        None
    };
    // "Has symbols" as the wire tells it: an outline body arrived, or the
    // outline is suppressed (symbols exist behind the flag). A covered file
    // with neither has no symbols; an uncovered file could not be enriched —
    // both carry the `no outline` marker, exactly the retired executor's
    // spelling for each.
    let has_symbols = suppressed || outline.is_some();
    let mark_no_outline = node.entry.line_count.is_some() && !has_symbols;
    let descriptor = file_descriptor(&node.entry, mark_no_outline);
    let flags = compute_entry_flags(has_symbols, suppressed, node.entry.is_gitignored);
    render_entry_line(out, &node.entry, &descriptor, &flags, node.entry_indent);
    if let Some(body) = outline {
        for line in body.lines() {
            let _ = writeln!(out, "{}{line}", node.outline_indent);
        }
    }
}

/// Counts the filesystem paths a glob query resolves to (`--count`).
///
/// Under the one-verb form every positional is a **pattern**, so the count is
/// the pattern's match set — each match counted once, file or directory alike
/// (misc 184: one pattern, one set, one number). The listing legitimately
/// descends into a matched directory to render its contents; the count must
/// not, or the two surfaces disagree whenever a pattern matches directories.
///
/// LSP enrichment is skipped — a count is pure filesystem, and reads **zero**
/// file content bytes. Run CLI-side since the ws43-03 cutover (no daemon
/// round-trip; the connection, if any, is dropped unused).
#[must_use]
pub fn count_glob_paths(
    fs_manager: &FilesystemManager,
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
    exclude: &ExcludeSet,
    cancel: &CancellationToken,
) -> usize {
    // Resolve every positional as a pattern (the one-verb form), apply the
    // exclude to each match set, and tally the survivors. This mirrors the
    // rendered listing's match set exactly, so `--count` and the listing
    // agree under an exclude (bug 73).
    let mut groups =
        expand_glob_patterns_grouped_cancellable(paths, include_gitignored, include_hidden, cancel);
    apply_exclude_to_groups(fs_manager, &mut groups, exclude);
    let mut total = 0usize;
    for group in &groups {
        if cancel.is_cancelled() {
            break;
        }
        total += group.resolved.len();
    }
    total
}

/// Collects the immediate children of a directory as `GlobEntry` rows.
///
/// Applies the visibility (hidden), gitignore, and `exclude` filters and
/// detects per-entry flags (gitignored, snapshot, symlink, broken). Shared by
/// [`plan_dir`] (which lays out the rows) and the `--count` walk's tests so
/// the two never diverge. `canonical` must be the canonicalized directory
/// path.
///
/// `cancel` is checked per entry; the CLI passes a fresh token (a killed CLI
/// process simply dies), and the parameter keeps the walk's bounded-work tests
/// honest.
#[allow(clippy::too_many_lines, reason = "sequential per-entry classification")]
fn collect_dir_entries(
    fs_manager: &FilesystemManager,
    canonical: &Path,
    include_gitignored: bool,
    include_hidden: bool,
    exclude: &ExcludeSet,
    cancel: &CancellationToken,
) -> Result<Vec<GlobEntry>> {
    // Build non-gitignored set for flag detection.
    let non_ignored: HashSet<PathBuf> = if include_gitignored {
        WalkBuilder::new(canonical)
            .max_depth(Some(1))
            .git_ignore(true)
            .hidden(!include_hidden)
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .collect()
    } else {
        HashSet::new()
    };

    let walker = WalkBuilder::new(canonical)
        .max_depth(Some(1))
        .git_ignore(!include_gitignored)
        .hidden(!include_hidden)
        .build();

    let mut entries = Vec::new();

    for entry in walker.flatten() {
        #[cfg(test)]
        probe::COLLECT_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if cancel.is_cancelled() {
            break;
        }
        let entry_path = entry.into_path();
        if entry_path.as_path() == canonical {
            continue;
        }

        let name = entry_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Apply exclude filter against the entry path.
        if exclude.is_match(&entry_path, canonical) {
            continue;
        }

        let is_gitignored = include_gitignored && !non_ignored.contains(&entry_path);
        let is_snap = is_snapshot(&name);

        let metadata = entry_path
            .symlink_metadata()
            .map_err(|e| anyhow!("Failed to read metadata for {name}: {e}"))?;

        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&entry_path)
                .map_or_else(|_| "?".to_string(), |t| t.to_string_lossy().to_string());
            let resolved_meta = std::fs::metadata(&entry_path).ok();
            let is_broken = resolved_meta.is_none();

            let (line_count, binary_size) = if is_broken || is_snap {
                (None, None)
            } else {
                file_info(fs_manager, &entry_path, resolved_meta.as_ref())
            };

            entries.push(GlobEntry {
                name,
                abs_path: entry_path,
                is_dir: resolved_meta
                    .as_ref()
                    .is_some_and(std::fs::Metadata::is_dir),
                line_count,
                binary_size,
                is_symlink: true,
                symlink_target: Some(target),
                is_broken_symlink: is_broken,
                is_gitignored,
                is_snapshot: is_snap,
            });
        } else if metadata.is_dir() {
            entries.push(GlobEntry {
                name: format!("{name}/"),
                abs_path: entry_path,
                is_dir: true,
                line_count: None,
                binary_size: None,
                is_symlink: false,
                symlink_target: None,
                is_broken_symlink: false,
                is_gitignored,
                is_snapshot: false,
            });
        } else {
            let (line_count, binary_size) = if is_snap {
                (None, None)
            } else {
                file_info(fs_manager, &entry_path, Some(&metadata))
            };
            entries.push(GlobEntry {
                name,
                abs_path: entry_path,
                is_dir: false,
                line_count,
                binary_size,
                is_symlink: false,
                symlink_target: None,
                is_broken_symlink: false,
                is_gitignored,
                is_snapshot: is_snap,
            });
        }
    }

    Ok(entries)
}

/// Extracts file info: `(line_count, binary_size)`.
fn file_info(
    fs_manager: &FilesystemManager,
    path: &Path,
    metadata: Option<&std::fs::Metadata>,
) -> (Option<usize>, Option<String>) {
    #[cfg(test)]
    probe::FILE_INFO_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    metadata.map_or((None, None), |m| {
        fs_manager.line_count(path, m).map_or_else(
            || (None, Some(format_file_size(m.len()))),
            |lc| (Some(lc), None),
        )
    })
}

// ─── Outline eligibility ──────────────────────────────────────────────

/// Returns `true` if the file matches any `outline_suppress` pattern.
pub(super) fn is_outline_suppressed(
    abs_path: &Path,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> bool {
    if outline_suppress.is_empty() {
        return false;
    }
    let rel = fs_manager
        .resolve_root(abs_path)
        .and_then(|root| abs_path.strip_prefix(&root).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| abs_path.to_path_buf());
    outline_suppress.iter().any(|pat| pat.is_match(&rel))
}

/// Returns `true` if the filename matches the snapshot sidecar pattern.
fn is_snapshot(name: &str) -> bool {
    name.contains(".catenary_snapshot_")
}

// ─── Outline kind filter (types and callables only) ──────────────────

/// Whether `kind` is a **container** the outline recurses into.
///
/// Two families, per the types-and-callables ruling (misc 117): module-like
/// (`Module`/`Namespace`/`Package`) and type/impl (`Class`/`Interface`/`Enum`/
/// `Struct`/`Object` — rust-analyzer emits `impl` blocks as `Object`). The
/// outline descends through these at every depth; anything else terminates the
/// descent. Kind strings are the [`symbol_kind_to_string`] taxonomy.
///
/// [`symbol_kind_to_string`]: crate::symbol_index::symbol_kind_to_string
fn is_container_kind(kind: &str) -> bool {
    matches!(
        kind,
        "module" | "namespace" | "package" | "class" | "interface" | "enum" | "struct" | "object"
    )
}

/// Whether `kind` is a **callable** the outline shows but never enters.
///
/// `Function`/`Method`/`Constructor` render as a single line; their interior
/// (locals, loop vars, nested defs) is never descended into (misc 117).
fn is_callable_kind(kind: &str) -> bool {
    matches!(kind, "function" | "method" | "constructor")
}

// ─── Symbol rendering ─────────────────────────────────────────────────

/// Renders a single outline node: `{indent}{line}  <declaration source line>`.
///
/// The declaration source line (keyed by the symbol's `selectionRange` start,
/// not `range.start` which would land on a leading `///`/attribute line)
/// carries the kind implicitly (`fn foo(...)`, `struct Bar`, `# Heading`), so
/// no `<Kind>` label is rendered, and no `SymbolKind` ever surfaces. Nesting is
/// shown by `indent` (tree depth), so the old `/` has-children collapse marker
/// is gone — the children are expanded on their own indented lines. When the
/// source line is unavailable (file unreadable or line out of range) the bare
/// name is used so the node is never empty.
///
/// `pub(super)` since ws43-03: the hitstream annotator renders listing-weight
/// bodies (top-level nodes only) through this same line shape.
pub(super) fn render_symbol_line(
    out: &mut String,
    sym: &Symbol,
    indent: &str,
    source: Option<&str>,
) {
    let text = source.map_or_else(|| sym.name.as_str(), str::trim_end);
    let _ = writeln!(out, "{indent}{}  {text}", sym.line + 1);
}

/// Renders one file's outline as a **map of types and callables**, re-indented
/// by tree depth (misc 117).
///
/// `syms` are the file's symbols at every depth, in ascending declaration-line
/// order (as the index stores them). `documentSymbol` ranges nest — a child's
/// `[line, end_line]` span lies within its parent's — so an interval stack
/// recovers each node's depth: pop every ancestor whose span ends before this
/// node begins, and the remaining stack height is the depth. Shown nodes are
/// indented `base_indent` + one tab per depth level (glob normalizes structure
/// to tree depth, not source columns). The file is read once via
/// [`SourceLines`] for each node's declaration text.
///
/// The outline is a map, not a mirror: it renders **types and callables only**.
/// The filter, applied at every depth:
/// - **Top level (depth 0) shows everything** — a module-level `const` /
///   `static` / assignment is real API surface.
/// - **Below top level**, a node is shown only when every ancestor is a
///   [container](is_container_kind) (recursion descends through containers
///   only) and the node itself is a container or a [callable](is_callable_kind).
///   This prunes data members (`Field`/`Property`/`EnumMember`/`Variable`/
///   `Constant`) and never enters a callable's interior (locals, loop vars,
///   nested defs — a callable renders as one line).
///
/// Because every ancestor of a shown node is itself a shown container, the
/// display depth of a shown node equals its span depth, so the interval stack's
/// height still yields the correct indent. Filtering happens at the render
/// only: the symbol index stays complete (grep's `#scope` enrichment and symbol
/// queries need the full tree).
///
/// `pub(super)` since ws43-03: the daemon-side hitstream annotator renders
/// `--outline` (full-weight) bodies through this, with an empty base indent —
/// the CLI re-indents each line for its listing position.
pub(super) fn render_full_outline(
    out: &mut String,
    file: &Path,
    syms: &[Symbol],
    base_indent: &str,
    sources: &mut SourceLines,
) {
    // Stack of open ancestors: `(end_line, is_container)`. The container flag
    // drives the types-and-callables filter — a node below top level is shown
    // only when every ancestor is a container.
    let mut open: Vec<(u32, bool)> = Vec::new();
    for sym in syms {
        while open.last().is_some_and(|&(end, _)| end < sym.line) {
            open.pop();
        }
        let depth = open.len();
        let container = is_container_kind(&sym.kind);
        let show = depth == 0
            || (open.iter().all(|&(_, c)| c) && (container || is_callable_kind(&sym.kind)));
        if show {
            let indent = format!("{base_indent}{}", "\t".repeat(depth));
            let source = sources.line(file, sym.line);
            render_symbol_line(out, sym, &indent, source);
        }
        open.push((sym.end_line, container));
    }
}

/// Returns the file's parenthetical descriptor: `(N line[s])`, with `, no
/// outline` appended when `mark_no_outline` (a text file whose language has no
/// server, or whose `documentSymbol` produced nothing), or `(size)` for a
/// binary file (never marked — a binary has no outline by nature). Empty when
/// the file has neither a line count nor a size.
fn file_descriptor(entry: &GlobEntry, mark_no_outline: bool) -> String {
    entry.binary_size.as_ref().map_or_else(
        || match entry.line_count {
            Some(lc) if mark_no_outline => format!("({}, no outline)", pluralize_lines(lc)),
            Some(lc) => format!("({})", pluralize_lines(lc)),
            None => String::new(),
        },
        |size| format!("({size})"),
    )
}

/// Pluralizes a file's line count: `1 line`, `0 lines`, `N lines`.
fn pluralize_lines(count: usize) -> String {
    if count == 1 {
        "1 line".to_string()
    } else {
        format!("{count} lines")
    }
}

/// Builds a directory's `(N files, M dirs)` child-count suffix, or `(empty)`
/// when it has no entries. Pluralizes each part (`1 file`, `1 dir`).
fn dir_count_suffix(files: usize, dirs: usize) -> String {
    if files == 0 && dirs == 0 {
        return "(empty)".to_string();
    }
    let files_word = if files == 1 {
        "1 file".to_string()
    } else {
        format!("{files} files")
    };
    let dirs_word = if dirs == 1 {
        "1 dir".to_string()
    } else {
        format!("{dirs} dirs")
    };
    format!("({files_word}, {dirs_word})")
}

/// Counts a directory's immediate children split into `(files, dirs)`, applying
/// the same visibility (gitignore/hidden) and `exclude` filters the listing
/// uses — *what globbing into this directory would enumerate*, the preview of
/// the next glob. Cheap: one stat per entry, no content reads (unlike
/// [`collect_dir_entries`]), so previewing a huge `target/` does not
/// read 40 000 files. Symlinks are followed for the file/dir decision, matching
/// the listing's classification.
fn count_dir_children(
    dir: &Path,
    include_gitignored: bool,
    include_hidden: bool,
    exclude: &ExcludeSet,
) -> (usize, usize) {
    let walker = WalkBuilder::new(dir)
        .max_depth(Some(1))
        .git_ignore(!include_gitignored)
        .hidden(!include_hidden)
        .build();
    let mut files = 0usize;
    let mut dirs = 0usize;
    for entry in walker.flatten() {
        let entry_path = entry.into_path();
        if entry_path.as_path() == dir {
            continue;
        }
        if exclude.is_match(&entry_path, dir) {
            continue;
        }
        if std::fs::metadata(&entry_path).is_ok_and(|m| m.is_dir()) {
            dirs += 1;
        } else {
            files += 1;
        }
    }
    (files, dirs)
}

// ─── Exclude filtering ────────────────────────────────────────────────

/// Drops each glob **pattern's** matched paths that the compiled `exclude`
/// selects, so a pattern argument honors `--exclude-pattern` exactly as a
/// named-directory argument does (bug 73).
///
/// Only pattern groups ([`ArgResolution::is_pattern`]) are filtered: naming a
/// file or directory is a direct request whose own path is never excluded — a
/// named directory's *entries* are filtered downstream by the listing
/// ([`collect_dir_entries`]) and count walks, so both argument kinds
/// surface the same surviving set. A no-op when `exclude` is `None`.
///
/// Shared by [`build_glob_plan`] (rendered listing) and
/// [`count_glob_paths`] (`--count`) so the two never diverge, and applied
/// before the flat set feeds glob's scoped-nudge observations so the nudge
/// tracks only the files the query surfaces. A pattern whose every match is
/// excluded is left with an empty `resolved`, so it renders the honest
/// `no matches for pattern` report rather than vanishing.
///
/// A path is dropped when **any** pattern in the set matches it, so every
/// collected `--exclude-pattern` reaches this leg (bug 89) exactly as it reaches
/// the grep walk and the named-dir entry filter — no pattern silently dropped
/// (the bug-73 leak class).
fn apply_exclude_to_groups(
    fs_manager: &FilesystemManager,
    groups: &mut [ArgResolution],
    exclude: &ExcludeSet,
) {
    if exclude.is_empty() {
        return;
    }
    for group in groups.iter_mut().filter(|g| g.is_pattern) {
        group
            .resolved
            .retain(|path| !path_matches_exclude(fs_manager, exclude, path));
    }
}

/// Whether any pattern in `exclude` selects `path`, resolving the root a
/// relative pattern strips.
///
/// An absolute exclude matches the full path (the root is ignored); a basename
/// exclude (the `**/<name>` form the router expands a no-slash pattern into) is
/// root-relative, so the path's owning workspace root is stripped first —
/// falling back to the path's parent, then the path itself, when no root owns
/// it. `**/<name>` is depth-independent, so any ancestor root yields the same
/// verdict. This mirrors the entry-level filter
/// [`collect_dir_entries`] and the grep walk apply, so a
/// pattern-matched path is excluded on the same terms as a directory entry.
fn path_matches_exclude(fs_manager: &FilesystemManager, exclude: &ExcludeSet, path: &Path) -> bool {
    let root = fs_manager
        .resolve_root(path)
        .or_else(|| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf());
    exclude.is_match(path, &root)
}

// ─── Entry rendering ─────────────────────────────────────────────────

/// Computes the appended `[…]` flags for an entry: `symbols available` when the
/// file has symbols that are suppressed from display, and `gitignored`.
/// Broken/snapshot entries render their own dedicated form in
/// [`render_entry_line`] and ignore these flags.
fn compute_entry_flags<'a>(has_symbols: bool, suppressed: bool, gitignored: bool) -> Vec<&'a str> {
    let mut flags = Vec::new();
    if has_symbols && suppressed {
        flags.push("symbols available");
    }
    if gitignored {
        flags.push("gitignored");
    }
    flags
}

/// Renders a `GlobEntry`'s header line: `{indent}{name}  {descriptor}{flags}`.
///
/// `descriptor` is the precomputed parenthetical — `(N lines)`, `(N lines, no
/// outline)`, `(size)`, or a directory's `(N files, M dirs)`/`(empty)` — or
/// empty. Broken symlinks and snapshot sidecars render their own dedicated form
/// (and carry no descriptor); a symlink renders `name -> target` before the
/// descriptor.
fn render_entry_line(
    out: &mut String,
    entry: &GlobEntry,
    descriptor: &str,
    flags: &[&str],
    indent: &str,
) {
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };

    if entry.is_broken_symlink {
        let target = entry.symlink_target.as_deref().unwrap_or("?");
        let _ = writeln!(out, "{indent}{} -> {target} [broken]", entry.name);
    } else if entry.is_snapshot {
        let _ = writeln!(out, "{indent}{} [snapshot]", entry.name);
    } else if entry.is_symlink {
        let target = entry.symlink_target.as_deref().unwrap_or("?");
        if descriptor.is_empty() {
            let _ = writeln!(out, "{indent}{} -> {target}{flag_str}", entry.name);
        } else {
            let _ = writeln!(
                out,
                "{indent}{} -> {target}  {descriptor}{flag_str}",
                entry.name
            );
        }
    } else if descriptor.is_empty() {
        let _ = writeln!(out, "{indent}{}{flag_str}", entry.name);
    } else {
        let _ = writeln!(out, "{indent}{}  {descriptor}{flag_str}", entry.name);
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

/// Whether `path` is a regular file or a symlink, retrying a transient miss.
///
/// A fresh `is_file()`/`is_symlink()` can transiently fail when an atomic-rename
/// write replaces the entry between `expand_search_paths` (which already
/// confirmed the path present) and this dispatch. Retrying a bounded number of
/// times (no sleep — the rename window is sub-millisecond) avoids silently
/// skipping a named file that is present on disk.
fn path_is_file_or_symlink_with_retry(path: &Path) -> bool {
    path_is_file_or_symlink_with_retry_with(path, STAT_RETRY_ATTEMPTS, |p| {
        p.is_file() || p.is_symlink()
    })
}

/// Retry loop body for [`path_is_file_or_symlink_with_retry`], with the
/// per-attempt file/symlink probe injected.
///
/// The production helper calls this with the real `is_file() || is_symlink()`
/// probe and [`STAT_RETRY_ATTEMPTS`]; tests inject a stateful probe (miss on
/// attempt 1, hit thereafter) to prove the loop actually retries — a regression
/// to a single attempt would no longer recover a transient miss.
fn path_is_file_or_symlink_with_retry_with(
    path: &Path,
    attempts: u32,
    probe: impl Fn(&Path) -> bool,
) -> bool {
    for attempt in 0..attempts {
        if probe(path) {
            return true;
        }
        // Yield between attempts (not after the last) so the scheduler can advance
        // the racing writer past its sub-µs atomic-rename window before the
        // re-stat — back-to-back syscalls almost never straddle that window. Cheap
        // and `.await`-free (this is a sync helper). (walk-3)
        if attempt + 1 < attempts {
            std::thread::yield_now();
        }
    }
    false
}

/// Collects the `(absolute path, mtime)` observations for glob's scoped walk —
/// the files within the glob pattern (WS31 ticket 04).
///
/// For a resolved **file** path: the file itself. For a resolved **directory**
/// path: its immediate entries (max depth 1), honoring the query's
/// gitignore/hidden visibility and the `exclude` filter so the observation set
/// matches what the listing surfaces. Per-file stats are the portable
/// correctness path (a content edit advances the file mtime, not the parent dir
/// mtime). Unreadable entries are skipped.
///
/// Observations are keyed by each entry's **canonical** real path (falling back
/// to the literal path only if `canonicalize` fails) so they agree with grep's
/// (`WalkBuilder::new(root)`) and diagnostics' (`stat_walk`) walks, which run
/// with `follow_links` **off** and therefore never descend an in-tree
/// symlink-to-dir — they only ever observe the real path. Keying literally
/// would double-key the same physical file (`linkdir/x` here, `realdir/x`
/// there): the orphan literal entry is never re-observed by a non-following
/// walk and gets phantom-reaped `Deleted` (WS31-review F2; reverses the pass-1
/// "canonicalize-nowhere" call). A symlink target *outside* every root
/// canonicalizes outside → [`resolve_root`] in the caller returns `None` → the
/// entry is correctly dropped (following such a target is opt-in via
/// `--follow-links`, fs-coherence ticket 07).
fn collect_scoped_observations(
    resolved: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
    exclude: &ExcludeSet,
) -> Vec<(PathBuf, i64)> {
    let mut observed: Vec<(PathBuf, i64)> = Vec::new();
    for path in resolved {
        // Directories first — `is_dir()` follows symlinks, so a symlink-to-dir
        // routes here and is walked at its literal path; each entry is then
        // canonicalized to its real path so its rel key matches grep/diagnostics.
        // This dir-first order matches `handle_literal_paths` so the listing and
        // the nudge classify a symlink-to-dir the same way (WS31-review walk-2).
        if path.is_dir() {
            // Canonicalize the dir arg ONCE (resolving a symlink-to-dir and any
            // symlink prefix components). A direct, non-symlink child's real path
            // is then `canonical_dir.join(leaf)` — no per-entry `canonicalize`
            // syscall on top of the mandatory `metadata()` (WS31-review c1r-2).
            // Only a child that is *itself* a symlink still needs a per-entry
            // canonicalize to resolve its target. `None` when the dir itself
            // can't canonicalize → fall back to per-entry resolution.
            let canonical_dir = path.canonicalize().ok();
            let walker = WalkBuilder::new(path)
                .max_depth(Some(1))
                .git_ignore(!include_gitignored)
                .hidden(!include_hidden)
                .build();
            for entry in walker.flatten() {
                let entry_is_symlink = entry.path_is_symlink();
                let entry_path = entry.into_path();
                if entry_path.as_path() == path.as_path() {
                    continue;
                }
                if exclude.is_match(&entry_path, path) {
                    continue;
                }
                // Only regular files carry an mtime worth diffing; the per-file
                // stat is the correctness path. Key by the canonical real path.
                if let Ok(md) = std::fs::metadata(&entry_path)
                    && md.is_file()
                {
                    // A non-symlink child under an already-canonical dir: its real
                    // path is `canonical_dir/leaf`, no extra syscall. Otherwise
                    // (symlinked child, or the dir didn't canonicalize) resolve
                    // per-entry. A confirmed-present entry whose canonicalize
                    // fails is OMITTED, never literal-keyed — a scoped walk that
                    // drops an entry can't phantom-reap it, and the next clean
                    // glob re-observes the canonical key (WS31-review T2/F2).
                    let key = match (&canonical_dir, entry_is_symlink) {
                        (Some(dir), false) => entry_path
                            .file_name()
                            .map(|leaf| dir.join(leaf))
                            .or_else(|| canonical_key(&entry_path)),
                        _ => canonical_key(&entry_path),
                    };
                    if let Some(key) = key {
                        observed.push((key, mtime_nanos(&md)));
                    }
                }
            }
        } else if path_is_file_or_symlink_with_retry(path) {
            // An actual file or a symlink-to-file: record the canonical path.
            // A broken symlink stats as an error here and is skipped. A
            // confirmed-present file whose canonicalize fails is OMITTED, never
            // literal-keyed, so a later full walk can't phantom-reap it
            // (WS31-review T2/F2).
            if let Ok(md) = std::fs::metadata(path)
                && let Some(key) = canonical_key(path)
            {
                observed.push((key, mtime_nanos(&md)));
            }
        }
    }
    observed
}

/// Canonicalizes an observed entry's path to its real path, returning `None`
/// when `canonicalize` fails.
///
/// Used to key glob's changed-set observations by the same real path
/// grep/diagnostics' non-following walks produce, so the same physical file is
/// never double-keyed (WS31-review F2). The caller invokes this only for an entry
/// it has already confirmed present (metadata `Ok`), so a `canonicalize` failure
/// here (EACCES on a parent, a symlink component swapped mid-walk, or a TOCTOU
/// removal) must NOT fall back to the literal path: literal-keying a
/// link-traversed orphan re-creates F2 (the orphan is never re-observed by a
/// non-following walk and is phantom-reaped `Deleted`). Returning `None` makes
/// the caller OMIT the observation — a scoped walk that drops an entry cannot
/// phantom-reap it, and the next clean glob re-observes the canonical key
/// (WS31-review T2).
fn canonical_key(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

/// Whether a path component carries a glob metacharacter (`* ? [ {`).
///
/// Mirrors [`ResolvedGlob::base_dir`](crate::bridge::session::ResolvedGlob)'s
/// split point (`crate::bridge::session`), so the metachar-free prefix this
/// helper canonicalizes lines up with the base the expansion walk actually roots
/// at.
fn component_has_metachar(component: &std::ffi::OsStr) -> bool {
    let s = component.to_string_lossy();
    s.contains(['*', '?', '[', '{'])
}

/// Canonicalizes a glob pattern's metachar-free base, preserving the glob
/// remainder, and falls back to the raw pattern when the base can't be resolved.
///
/// This is glob's ingestion-seam canonicalization (misc 193, mirroring grep's
/// cwd canonicalization in `b9145d5`). The daemon tracks canonical roots, but the
/// host passes its raw pattern spelling; under a symlinked prefix the two differ,
/// so a raw base makes the expansion walk emit raw-spelled paths that fail
/// `resolve_root`'s canonical prefix check — collapsing enrichment and the scoped
/// nudge. The pattern is split at its first metachar-bearing component (the same
/// point `ResolvedGlob::base_dir` uses to root the walk); the metachar-free
/// prefix is canonicalized and the glob remainder re-appended verbatim.
///
/// A relative pattern (no absolute base) or one whose base does not yet exist
/// keeps its spelling (`canonicalize` can't resolve it) — a zero-match pattern
/// still reports honestly, and a relative pattern carries no base to resolve.
#[must_use]
pub fn canonicalize_pattern_base(pattern: &Path) -> PathBuf {
    // Split into the metachar-free prefix (the walk's base) and the glob
    // remainder, mirroring `ResolvedGlob::base_dir`.
    let mut base = PathBuf::new();
    let mut remainder = PathBuf::new();
    let mut in_remainder = false;
    for component in pattern.components() {
        if in_remainder || component_has_metachar(component.as_os_str()) {
            in_remainder = true;
            remainder.push(component);
        } else {
            base.push(component);
        }
    }

    // A pattern that is metachar-first (no metachar-free prefix) carries no base
    // to canonicalize — return it unchanged.
    if base.as_os_str().is_empty() {
        return pattern.to_path_buf();
    }

    // Canonicalize the base once; on failure keep the raw pattern (a not-yet-
    // existing base, or a relative one canonicalize can't resolve).
    let Ok(canonical_base) = base.canonicalize() else {
        return pattern.to_path_buf();
    };
    if remainder.as_os_str().is_empty() {
        canonical_base
    } else {
        canonical_base.join(remainder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::Glob;

    // ─── canonicalize_pattern_base — glob's ingestion-seam canonicalization (misc 193) ──

    /// A fully-literal absolute pattern reached through a symlinked prefix
    /// component canonicalizes to its real path — the spelling `resolve_root`,
    /// the display, and enrichment all agree on.
    #[test]
    #[cfg(unix)]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn canonicalize_pattern_base_resolves_symlinked_prefix() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        let realdir = base.join("realdir");
        std::fs::create_dir(&realdir).expect("create realdir");
        let real_file = realdir.join("x.rs");
        std::fs::write(&real_file, "fn x() {}\n").expect("write file");
        let linkdir = base.join("linkdir");
        symlink(&realdir, &linkdir).expect("create linkdir symlink");

        // A file pattern reached through the symlinked prefix.
        let raw = linkdir.join("x.rs");
        assert_eq!(
            canonicalize_pattern_base(&raw),
            real_file,
            "a literal pattern under a symlinked prefix must canonicalize to its \
             real path"
        );
    }

    /// The metachar-free base is canonicalized; the glob remainder is preserved
    /// verbatim (the base is where `resolve_root`/enrichment key, the remainder
    /// is the walk's matcher).
    #[test]
    #[cfg(unix)]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn canonicalize_pattern_base_preserves_glob_remainder() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        let realdir = base.join("realdir");
        std::fs::create_dir(&realdir).expect("create realdir");
        let linkdir = base.join("linkdir");
        symlink(&realdir, &linkdir).expect("create linkdir symlink");

        // `<linkdir>/**/*.rs`: base `<linkdir>` canonicalizes, `**/*.rs` rides along.
        let pattern = linkdir.join("**").join("*.rs");
        let expected = realdir.join("**").join("*.rs");
        assert_eq!(
            canonicalize_pattern_base(&pattern),
            expected,
            "the metachar-free base canonicalizes while the glob remainder is kept"
        );
    }

    /// A not-yet-existing base keeps its raw spelling (canonicalize can't resolve
    /// it), so a zero-match pattern still reports honestly rather than erroring.
    #[test]
    fn canonicalize_pattern_base_absent_base_keeps_spelling() {
        let pattern = Path::new("/no/such/dir/*.rs");
        assert_eq!(
            canonicalize_pattern_base(pattern),
            pattern.to_path_buf(),
            "an absent base must keep its raw spelling"
        );
    }

    /// A relative pattern whose base does not exist relative to the daemon cwd
    /// keeps its spelling (canonicalize can't resolve it). The router absolutizes
    /// relative patterns before dispatch, so this only guards the defensive path;
    /// a clearly-nonexistent base keeps the assertion independent of the test's
    /// working directory.
    #[test]
    fn canonicalize_pattern_base_relative_pattern_unchanged() {
        let pattern = Path::new("no_such_relative_base_193/**/*.rs");
        assert_eq!(
            canonicalize_pattern_base(pattern),
            pattern.to_path_buf(),
            "a relative pattern with an absent base is returned unchanged"
        );
    }

    #[test]
    fn ws31_review_r2_live_retry_recovers_transient_miss() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A stateful probe that misses on call 1 and hits on every later call —
        // the deterministic transient miss→hit a real atomic-rename race would
        // produce. With the full `STAT_RETRY_ATTEMPTS` budget the loop must
        // recover; with a single attempt it must NOT — so the guard is sensitive
        // to the retry count (a regression to `attempts == 1` fails here, where a
        // terminal file/absent test would still pass).
        const {
            assert!(
                STAT_RETRY_ATTEMPTS >= 2,
                "the retry guard assumes more than one attempt"
            );
        }

        let calls = AtomicUsize::new(0);
        let probe = |_: &Path| calls.fetch_add(1, Ordering::Relaxed) >= 1;
        let path = Path::new("/does/not/matter");

        assert!(
            path_is_file_or_symlink_with_retry_with(path, STAT_RETRY_ATTEMPTS, probe),
            "the bounded retry must recover a miss that resolves on a later attempt"
        );

        // Same probe, fresh counter, single attempt: the first call misses and
        // there is no retry, so the loop reports absent — pinning the retry-count
        // sensitivity (a `STAT_RETRY_ATTEMPTS = 1` regression would surface here).
        let calls = AtomicUsize::new(0);
        let probe = |_: &Path| calls.fetch_add(1, Ordering::Relaxed) >= 1;
        assert!(
            !path_is_file_or_symlink_with_retry_with(path, 1, probe),
            "a single attempt cannot recover a transient miss"
        );
    }

    // ─── entry construction helper ───────────────────────────────────

    fn make_glob_entry(
        name: &str,
        abs_path: &Path,
        is_dir: bool,
        line_count: Option<usize>,
    ) -> GlobEntry {
        GlobEntry {
            name: name.to_string(),
            abs_path: abs_path.to_path_buf(),
            is_dir,
            line_count,
            binary_size: None,
            is_symlink: false,
            symlink_target: None,
            is_broken_symlink: false,
            is_gitignored: false,
            is_snapshot: false,
        }
    }

    // ─── pluralization + descriptors ─────────────────────────────────

    #[test]
    fn test_pluralize_lines() {
        // The `(1 lines)` bug fixed: a single line is singular.
        assert_eq!(pluralize_lines(0), "0 lines");
        assert_eq!(pluralize_lines(1), "1 line");
        assert_eq!(pluralize_lines(2), "2 lines");
        assert_eq!(pluralize_lines(92), "92 lines");
    }

    // ─── exclude filtering (bug 73) ──────────────────────────────────

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn apply_exclude_to_groups_filters_patterns_not_named_args() {
        // A glob pattern's matches are filtered by the exclude; a directly-named
        // argument (is_pattern == false) is a direct request and stays, even when
        // its own path matches the exclude — a named directory's entries are
        // filtered downstream, not the argument itself (bug 73).
        let fs = FilesystemManager::new();
        let exclude = ExcludeSet::compile(&["**/*.rs".to_string()]).expect("compile exclude");

        let mut groups = vec![
            ArgResolution {
                resolved: vec![
                    PathBuf::from("/root/a.rs"),
                    PathBuf::from("/root/keep.txt"),
                    PathBuf::from("/root/sub/b.rs"),
                ],
                is_pattern: true,
            },
            ArgResolution {
                resolved: vec![PathBuf::from("/root/named.rs")],
                is_pattern: false,
            },
        ];

        apply_exclude_to_groups(&fs, &mut groups, &exclude);

        // Pattern group: every `.rs` match dropped, the `.txt` survives.
        assert_eq!(
            groups[0].resolved,
            vec![PathBuf::from("/root/keep.txt")],
            "pattern matches honor the exclude"
        );
        // Named argument: untouched even though it matches the exclude.
        assert_eq!(
            groups[1].resolved,
            vec![PathBuf::from("/root/named.rs")],
            "a directly-named argument is never excluded"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn apply_exclude_to_groups_unions_multiple_patterns() {
        // A repeated `--exclude-pattern` (bug 89) unions its patterns: a
        // pattern-match is dropped when ANY compiled exclude selects it. Both
        // patterns must reach the filter — the bug-73 leak class at the group
        // level. `keep.md` matches neither and survives.
        let fs = FilesystemManager::new();
        let exclude = ExcludeSet::compile(&["**/*.rs".to_string(), "**/*.txt".to_string()])
            .expect("compile exclude");

        let mut groups = vec![ArgResolution {
            resolved: vec![
                PathBuf::from("/root/a.rs"),
                PathBuf::from("/root/b.txt"),
                PathBuf::from("/root/keep.md"),
            ],
            is_pattern: true,
        }];

        apply_exclude_to_groups(&fs, &mut groups, &exclude);

        assert_eq!(
            groups[0].resolved,
            vec![PathBuf::from("/root/keep.md")],
            "both excludes apply; only the unmatched path survives"
        );
    }

    #[test]
    fn apply_exclude_to_groups_empty_set_is_noop() {
        let fs = FilesystemManager::new();
        let mut groups = vec![ArgResolution {
            resolved: vec![PathBuf::from("/root/a.rs")],
            is_pattern: true,
        }];
        apply_exclude_to_groups(&fs, &mut groups, &ExcludeSet::default());
        assert_eq!(groups[0].resolved, vec![PathBuf::from("/root/a.rs")]);
    }

    #[test]
    fn test_dir_count_suffix() {
        // `(empty)` for zero; each part pluralized independently.
        assert_eq!(dir_count_suffix(0, 0), "(empty)");
        assert_eq!(dir_count_suffix(1, 0), "(1 file, 0 dirs)");
        assert_eq!(dir_count_suffix(0, 1), "(0 files, 1 dir)");
        assert_eq!(dir_count_suffix(3, 2), "(3 files, 2 dirs)");
        assert_eq!(dir_count_suffix(40000, 200), "(40000 files, 200 dirs)");
    }

    #[test]
    fn test_file_descriptor_text_and_no_outline() {
        let entry = make_glob_entry("f.rs", Path::new("/t/f.rs"), false, Some(1));
        // A text file with symbols: bare line count, singular.
        assert_eq!(file_descriptor(&entry, false), "(1 line)");
        // No symbols (no server / failed / empty): the `no outline` marker.
        assert_eq!(file_descriptor(&entry, true), "(1 line, no outline)");
    }

    #[test]
    fn test_file_descriptor_binary_never_marked() {
        let mut entry = make_glob_entry("d.bin", Path::new("/t/d.bin"), false, None);
        entry.binary_size = Some("1.5 MB".to_string());
        // A binary has no outline by nature — never the `no outline` marker.
        assert_eq!(file_descriptor(&entry, true), "(1.5 MB)");
    }

    // ─── is_outline_suppressed ──────────────────────────────────────

    #[test]
    fn test_outline_suppressed_empty_list() {
        let path = PathBuf::from("/test/file.rs");
        let fs = FilesystemManager::new();

        assert!(
            !is_outline_suppressed(&path, &[], &fs),
            "empty suppression list should not suppress"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_outline_suppressed_matching_pattern() {
        let path = PathBuf::from("/test/file.rs");
        let suppress = vec![
            Glob::new("**/*.rs")
                .expect("compile glob")
                .compile_matcher(),
        ];
        let fs = FilesystemManager::new();

        assert!(
            is_outline_suppressed(&path, &suppress, &fs),
            "matching pattern should suppress"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_outline_suppressed_non_matching_pattern() {
        let path = PathBuf::from("/test/file.rs");
        let suppress = vec![
            Glob::new("**/*.py")
                .expect("compile glob")
                .compile_matcher(),
        ];
        let fs = FilesystemManager::new();

        assert!(
            !is_outline_suppressed(&path, &suppress, &fs),
            "non-matching pattern should not suppress"
        );
    }

    // ─── render_symbol_line (declaration line, no kind, no slash) ───

    #[test]
    fn test_render_symbol_line_basic() {
        let sym = Symbol {
            name: "my_func".to_string(),
            kind: "function".to_string(),
            line: 9,
            end_line: 19,
            scope: None,
            scope_kind: None,
            deprecated: false,
        };

        let mut out = String::new();
        // The node is `{indent}{1-based line}  {declaration line}` — no colon,
        // no `<Kind>`, no trailing `/` (nesting is shown by indentation).
        render_symbol_line(&mut out, &sym, "\t", Some("fn my_func(x: u32) -> u32 {"));

        assert_eq!(out, "\t10  fn my_func(x: u32) -> u32 {\n");
    }

    #[test]
    fn test_render_symbol_line_no_kind_label_or_slash() {
        // A name-embedding server (lattice `H1:`) — the heading source line is
        // clean, so there is no `<Class>` label, and a container is no longer
        // marked with a trailing `/` (its children are expanded instead).
        let sym = Symbol {
            name: "H1: Parent".to_string(),
            kind: "class".to_string(),
            line: 0,
            end_line: 9,
            scope: None,
            scope_kind: None,
            deprecated: true,
        };

        let mut out = String::new();
        render_symbol_line(&mut out, &sym, "\t", Some("# Parent"));

        assert_eq!(out, "\t1  # Parent\n");
        assert!(!out.contains('<'), "no kind label: {out:?}");
        assert!(!out.contains('/'), "no has-children slash marker: {out:?}");
    }

    #[test]
    fn test_render_symbol_line_trailing_whitespace_trimmed() {
        // Trailing whitespace on the source line is trimmed so the output is
        // byte-stable.
        let sym = Symbol {
            name: "old_fn".to_string(),
            kind: "function".to_string(),
            line: 4,
            end_line: 6,
            scope: None,
            scope_kind: None,
            deprecated: true,
        };

        let mut out = String::new();
        render_symbol_line(&mut out, &sym, "", Some("fn old_fn() {   "));

        assert_eq!(out, "5  fn old_fn() {\n");
    }

    #[test]
    fn test_render_symbol_line_falls_back_to_name() {
        // When the source line is unavailable (file unreadable, line out of
        // range) the node renders the bare name rather than an empty atom.
        let sym = Symbol {
            name: "standalone".to_string(),
            kind: "function".to_string(),
            line: 0,
            end_line: 5,
            scope: None,
            scope_kind: None,
            deprecated: false,
        };

        let mut out = String::new();
        render_symbol_line(&mut out, &sym, "", None);

        assert_eq!(out, "1  standalone\n");
    }

    // ─── render_full_outline (fully expanded, depth-indented) ───────

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_render_full_outline_indents_by_tree_depth() {
        // Outer (lines 0–2) contains inner (line 1); leaf (line 3) is top-level.
        // Full expansion: every node on its own line, re-indented by tree depth
        // (one tab per level), no `<Kind>` label, no `/` collapse marker.
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("code.rs");
        std::fs::write(&file, "struct Outer {\nfn inner() {}\n}\nfn leaf() {}\n").expect("write");

        let syms = vec![
            Symbol {
                name: "Outer".to_string(),
                kind: "struct".to_string(),
                line: 0,
                end_line: 2,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
            Symbol {
                name: "inner".to_string(),
                kind: "function".to_string(),
                line: 1,
                end_line: 1,
                scope: Some("Outer".to_string()),
                scope_kind: Some("struct".to_string()),
                deprecated: false,
            },
            Symbol {
                name: "leaf".to_string(),
                kind: "function".to_string(),
                line: 3,
                end_line: 3,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
        ];

        let mut out = String::new();
        let mut sources = SourceLines::new();
        render_full_outline(&mut out, &file, &syms, "", &mut sources);

        // Depth 0 → no indent; depth 1 (inner, under Outer) → one tab.
        assert_eq!(
            out,
            "1  struct Outer {\n\t2  fn inner() {}\n4  fn leaf() {}\n"
        );
        assert!(!out.contains('<'), "no kind label anywhere: {out:?}");
    }

    // ─── outline filter: types and callables only (misc 117) ────────

    /// Builds a `Symbol` for the filter tests. Source lines are unavailable
    /// (the path is synthetic), so `render_full_outline` renders the bare name —
    /// which is what these tests assert on.
    fn sym(name: &str, kind: &str, line: u32, end_line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: kind.to_string(),
            line,
            end_line,
            scope: None,
            scope_kind: None,
            deprecated: false,
        }
    }

    /// Renders a symbol tree through the outline filter and returns the output.
    fn outline(syms: &[Symbol]) -> String {
        let mut out = String::new();
        let mut sources = SourceLines::new();
        render_full_outline(
            &mut out,
            Path::new("/synthetic/filter.rs"),
            syms,
            "",
            &mut sources,
        );
        out
    }

    #[test]
    fn outline_prunes_locals_under_a_function() {
        // A function's interior is never entered: locals and loop vars vanish,
        // the function itself renders as one line (the Python field finding).
        let syms = vec![
            sym("compute", "function", 0, 6),
            sym("rng", "variable", 1, 1),
            sym("val", "variable", 2, 4),
            sym("acc", "variable", 3, 3),
        ];
        assert_eq!(outline(&syms), "1  compute\n");
    }

    #[test]
    fn outline_keeps_methods_under_an_impl() {
        // `Object` is rust-analyzer's `impl` kind — a container the outline
        // recurses into, so its methods (callables) are kept and indented.
        let syms = vec![
            sym("impl Widget", "object", 0, 10),
            sym("new", "method", 1, 3),
            sym("render", "method", 4, 8),
        ];
        assert_eq!(outline(&syms), "1  impl Widget\n\t2  new\n\t5  render\n");
    }

    #[test]
    fn outline_prunes_fields_and_enum_variants() {
        // Struct fields and enum variants are data members — pruned below top
        // level; the containers themselves stay.
        let syms = vec![
            sym("Point", "struct", 0, 3),
            sym("x", "field", 1, 1),
            sym("y", "field", 2, 2),
            sym("Color", "enum", 4, 7),
            sym("Red", "member", 5, 5),
            sym("Green", "member", 6, 6),
        ];
        assert_eq!(outline(&syms), "1  Point\n5  Color\n");
    }

    #[test]
    fn outline_prunes_associated_const_inside_impl() {
        // A `const` inside an impl is a member → pruned; a sibling method stays
        // (the owned judgment call from the ruling).
        let syms = vec![
            sym("impl Widget", "object", 0, 6),
            sym("MAX", "constant", 1, 1),
            sym("build", "method", 2, 5),
        ];
        assert_eq!(outline(&syms), "1  impl Widget\n\t3  build\n");
    }

    #[test]
    fn outline_keeps_module_recursion_at_depth_two() {
        // Module-like containers recurse at every depth: a function two modules
        // deep is kept and indented by its span depth.
        let syms = vec![
            sym("outer", "module", 0, 20),
            sym("inner", "module", 1, 19),
            sym("deep_fn", "function", 2, 3),
        ];
        assert_eq!(outline(&syms), "1  outer\n\t2  inner\n\t\t3  deep_fn\n");
    }

    #[test]
    fn outline_keeps_top_level_constant() {
        // Depth 0 shows everything — a module-level constant is API surface.
        let syms = vec![
            sym("MAX_SIZE", "constant", 0, 0),
            sym("helper", "function", 1, 3),
        ];
        assert_eq!(outline(&syms), "1  MAX_SIZE\n2  helper\n");
    }

    #[test]
    fn outline_does_not_enter_nested_defs_inside_a_function() {
        // A nested def (closure/inner function) lives in a callable's interior
        // and is never entered, even though it is itself a callable.
        let syms = vec![
            sym("outer_fn", "function", 0, 5),
            sym("nested_fn", "function", 1, 3),
            sym("local", "variable", 2, 2),
        ];
        assert_eq!(outline(&syms), "1  outer_fn\n");
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn outline_filter_leaves_symbol_index_complete_for_grep_enrichment() {
        // The filter is a RENDER-only concern: the symbol index still holds every
        // symbol (including the pruned local), so grep's `#scope` symbol-path
        // enrichment and symbol queries keep the full tree.
        let idx = crate::symbol_index::SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/synthetic/enrich.rs");
        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "compute",
                "kind": 12,
                "range": { "start": { "line": 0 }, "end": { "line": 3 } },
                "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } },
                "children": [{
                    "name": "local",
                    "kind": 13,
                    "range": { "start": { "line": 1 }, "end": { "line": 1 } },
                    "selectionRange": { "start": { "line": 1 }, "end": { "line": 1 } }
                }]
            }]),
        )
        .expect("populate");

        // The index is complete — the local is queryable even though the outline
        // prunes it.
        let all = idx
            .query(".*", Some(std::slice::from_ref(&path)))
            .expect("query all");
        let names: Vec<&str> = all.iter().map(|(_, s)| s.name.as_str()).collect();
        assert!(
            names.contains(&"compute"),
            "index keeps the function: {names:?}"
        );
        assert!(
            names.contains(&"local"),
            "index keeps the pruned local for grep enrichment: {names:?}"
        );

        // The outline of those same symbols prunes the local.
        let syms: Vec<Symbol> = all.into_iter().map(|(_, s)| s).collect();
        assert_eq!(outline(&syms), "1  compute\n");
    }

    // ─── compute_entry_flags ───────────────────────────────────────

    #[test]
    fn test_compute_entry_flags_empty_when_no_conditions() {
        assert!(compute_entry_flags(false, false, false).is_empty());
        // Symbols present but rendered (not suppressed) → no flag.
        assert!(compute_entry_flags(true, false, false).is_empty());
    }

    #[test]
    fn test_compute_entry_flags_symbols_available_when_suppressed() {
        // Symbols exist but display is suppressed → `[symbols available]`.
        assert_eq!(
            compute_entry_flags(true, true, false),
            vec!["symbols available"]
        );
    }

    #[test]
    fn test_compute_entry_flags_gitignored() {
        assert_eq!(compute_entry_flags(false, false, true), vec!["gitignored"]);
    }

    #[test]
    fn test_compute_entry_flags_compose_suppressed_and_gitignored() {
        assert_eq!(
            compute_entry_flags(true, true, true),
            vec!["symbols available", "gitignored"]
        );
    }

    // ─── render_entry_line ─────────────────────────────────────────

    #[test]
    fn test_render_entry_line_regular_file_with_descriptor() {
        let entry = make_glob_entry("main.rs", Path::new("/test/main.rs"), false, Some(42));

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "(42 lines)", &[], "");

        assert_eq!(out, "main.rs  (42 lines)\n");
    }

    #[test]
    fn test_render_entry_line_no_outline_descriptor() {
        // A degraded file carries the `no outline` marker inside the descriptor.
        let entry = make_glob_entry("data.txt", Path::new("/test/data.txt"), false, Some(5));

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "(5 lines, no outline)", &[], "");

        assert_eq!(out, "data.txt  (5 lines, no outline)\n");
    }

    #[test]
    fn test_render_entry_line_with_flags() {
        let entry = make_glob_entry("main.rs", Path::new("/test/main.rs"), false, Some(42));

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "(42 lines)", &["symbols available"], "");

        assert_eq!(out, "main.rs  (42 lines) [symbols available]\n");
    }

    #[test]
    fn test_render_entry_line_dir_count() {
        let entry = make_glob_entry("sub/", Path::new("/test/sub"), true, None);

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "(2 files, 1 dir)", &[], "\t");

        assert_eq!(out, "\tsub/  (2 files, 1 dir)\n");
    }

    #[test]
    fn test_render_entry_line_broken_symlink() {
        let mut entry = make_glob_entry("broken.rs", Path::new("/test/broken.rs"), false, Some(10));
        entry.is_broken_symlink = true;
        entry.symlink_target = Some("/nonexistent".to_string());

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "", &[], "");

        assert_eq!(out, "broken.rs -> /nonexistent [broken]\n");
    }

    #[test]
    fn test_render_entry_line_snapshot() {
        let mut entry = make_glob_entry("snap.rs", Path::new("/test/snap.rs"), false, Some(10));
        entry.is_snapshot = true;

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "", &[], "");

        assert_eq!(out, "snap.rs [snapshot]\n");
    }

    #[test]
    fn test_render_entry_line_symlink_with_descriptor() {
        let mut entry = make_glob_entry("link.rs", Path::new("/test/link.rs"), false, Some(50));
        entry.is_symlink = true;
        entry.symlink_target = Some("/real/file.rs".to_string());

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "(50 lines)", &[], "");

        assert_eq!(out, "link.rs -> /real/file.rs  (50 lines)\n");
    }

    #[test]
    fn test_render_entry_line_binary() {
        let mut entry = make_glob_entry("data.bin", Path::new("/test/data.bin"), false, None);
        entry.binary_size = Some("1.5 MB".to_string());

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "(1.5 MB)", &[], "");

        assert_eq!(out, "data.bin  (1.5 MB)\n");
    }

    #[test]
    fn test_render_entry_line_no_descriptor() {
        let entry = make_glob_entry("empty", Path::new("/test/empty"), false, None);

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "", &[], "");

        assert_eq!(out, "empty\n");
    }

    #[test]
    fn test_render_entry_line_indented() {
        let entry = make_glob_entry("nested.rs", Path::new("/test/nested.rs"), false, Some(10));

        let mut out = String::new();
        render_entry_line(&mut out, &entry, "(10 lines)", &[], "\t");

        assert_eq!(out, "\tnested.rs  (10 lines)\n");
    }

    // ─── collect_scoped_observations — canonicalization divergence (R5 L7) ──

    /// C1/F2 — for a symlinked directory arg, `collect_scoped_observations` must
    /// yield the contained file at its CANONICAL `realdir/x.<EXT>` path, matching
    /// grep's (`WalkBuilder::new(root)`) and diagnostics' (`stat_walk`) walks,
    /// which never descend an in-tree symlink-to-dir (`follow_links` off) and so
    /// observe only the real path.
    ///
    /// This REVERSES the pass-1 "canonicalize-nowhere" call (L7). The pass-1
    /// premise — that grep/diagnostics key the file under `linkdir/x.<EXT>` — was
    /// wrong: those walks never follow the in-tree link, so they only ever
    /// produce `realdir/x.<EXT>`. Keying glob's observation literally
    /// (`linkdir/x.<EXT>`) double-keyed the same physical file, and the orphan
    /// `linkdir/x.<EXT>` baseline entry was phantom-reaped by the next full walk
    /// (F2). The decided fix canonicalizes glob's observed entries to the real
    /// path, so all three surfaces agree on `realdir/x.<EXT>`.
    #[test]
    #[cfg(unix)]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn ws31_review_r5_symlinked_glob_arg_single_baseline_key() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("create tempdir");
        // Canonicalize the tempdir base once so the ONLY symlink in play is
        // `linkdir` (some platforms route `/tmp` through a symlink; on Linux it
        // is real, but canonicalizing the base keeps the comparison robust).
        let base = tmp.path().canonicalize().expect("canonicalize base");

        let realdir = base.join("realdir");
        std::fs::create_dir(&realdir).expect("create realdir");
        let real_file = realdir.join("x.ws31ext");
        std::fs::write(&real_file, "fn x\n").expect("write file");

        let linkdir = base.join("linkdir");
        symlink(&realdir, &linkdir).expect("create linkdir symlink");
        // The literal (link-traversed) path glob USED to record — the bug.
        let literal_file = linkdir.join("x.ws31ext");
        // The canonical path grep/diagnostics record — the single correct key.
        let canonical_file = realdir.join("x.ws31ext");

        // include_hidden / include_gitignored so neither visibility filter hides
        // the entry; the file itself is non-hidden, but this keeps it unambiguous.
        let observed = collect_scoped_observations(
            std::slice::from_ref(&linkdir),
            true,
            true,
            &ExcludeSet::default(),
        );

        // Regression guard: the contained file must be observed at its CANONICAL
        // `realdir/x.<EXT>` path (the grep/diagnostics baseline key) — glob
        // canonicalizes its entries so it matches. Pre-fix (C1/F2) it recorded
        // the literal link-traversed `linkdir/x.<EXT>` instead.
        assert!(
            observed.iter().any(|(p, _)| *p == canonical_file),
            "glob's scoped observation must record the contained file at its \
             CANONICAL path (realdir/x.<EXT>), matching grep/diagnostics' \
             non-following walks; got: {observed:?}"
        );

        // The divergence must be gone: no observation under the literal
        // link-traversed path once glob canonicalizes its entries.
        assert!(
            !observed.iter().any(|(p, _)| *p == literal_file),
            "no observation should surface under the literal link-traversed path \
             once glob canonicalizes its entries; got: {observed:?}"
        );
    }

    // ─── count_glob_paths — dispatch parity with the plan build (WS31-review D1) ──

    /// T1 (retargeted for the VERBS one-verb form) — a symlink-to-dir pattern is
    /// a single self-matching path, so `--count` is `1` (the match counted once),
    /// **not** the directory's listed entry count. Under always-pattern there is
    /// no name/pattern dir-first branch: a matched directory counts once and only
    /// the listing descends into it (the misc-184 ruling — one pattern, one set,
    /// one number). The old "count follows the link and counts N entries"
    /// expectation belonged to the name-based model, whose shape branch retired
    /// (VERBS Dispositions: the shape refinement is the 184 species).
    #[test]
    #[cfg(unix)]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn ws31_review_d_count_matches_listing_symlink_dir() {
        use std::os::unix::fs::symlink;

        const N: usize = 3;

        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");

        // A real dir with N files.
        let realdir = base.join("realdir");
        std::fs::create_dir(&realdir).expect("create realdir");
        for i in 0..N {
            std::fs::write(realdir.join(format!("f{i}.ws31ext")), "x\n").expect("write file");
        }

        // An in-tree symlink pointing at that dir.
        let linkdir = base.join("linkdir");
        symlink(&realdir, &linkdir).expect("create linkdir symlink");

        let fs_manager = FilesystemManager::new();
        let count = count_glob_paths(
            &fs_manager,
            &[linkdir],
            false,
            false,
            &ExcludeSet::default(),
            &tokio_util::sync::CancellationToken::new(),
        );

        assert_eq!(
            count, 1,
            "a self-matching directory pattern counts once (the match set), \
             not its {N} listed entries; only the listing descends. got {count}"
        );
    }

    /// T2 — `canonical_key`'s present-entry contract: it returns `Some(real)` on
    /// success and `None` on a canonicalize failure, so the caller OMITS an
    /// uncanonicalizable-but-present entry rather than literal-keying it (which
    /// would re-create the F2 phantom-reap). A deterministic canonicalize failure
    /// on a genuinely *present* file is not portably stageable in a unit test
    /// (it needs an EACCES-on-parent / mid-walk symlink swap), so this asserts
    /// the helper's failure path directly: an unresolvable path → `None`
    /// (the omit signal), a present real path → `Some(canonical)`
    /// (land-with-fix per the D1 spec).
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn ws31_review_d_present_uncanonicalizable_dropped() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");

        // Unresolvable path (no such entry): canonicalize fails → `None`. This is
        // the signal the caller turns into an OMIT — never a literal-keyed orphan
        // (the F2 phantom-reap the literal fallback used to cause).
        let missing = base.join("does_not_exist.ws31ext");
        assert_eq!(
            canonical_key(&missing),
            None,
            "an uncanonicalizable path must yield None (omit), not a literal key: {}",
            missing.display()
        );

        // A present real file canonicalizes to itself (the dir is already
        // canonical) → `Some(real)`, so a clean observation is still keyed.
        let real_file = base.join("present.ws31ext");
        std::fs::write(&real_file, "x\n").expect("write file");
        assert_eq!(
            canonical_key(&real_file),
            Some(real_file.clone()),
            "a present, resolvable path must yield Some(canonical): {}",
            real_file.display()
        );
    }

    // ─── zero-match pattern reporting (misc 118) ────────────────────

    /// The plan build flags a pattern that expanded to zero matches, so the
    /// CLI can report it loudly. A single unmatched pattern resolves to
    /// nothing (no nodes, no enrichment set), so this stays fast and
    /// server-free.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn plan_reports_zero_match_pattern() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");

        let fs_manager = FilesystemManager::new();
        // A single absolute pattern whose base dir exists but matches no file.
        let plan = build_glob_plan(
            &fs_manager,
            &base.join("*.nomatch118"),
            &ExcludeSet::default(),
            false,
            false,
        )
        .expect("build plan");

        let output = render_glob_plan(&plan, &HashMap::new());
        assert!(
            output.is_empty(),
            "zero-match pattern renders nothing: {output:?}"
        );
        assert!(plan.no_match, "the sole pattern is flagged as a no-match");
        assert!(
            plan.enrich_files.is_empty(),
            "nothing to enrich on a zero-match"
        );
    }

    // ─── the ruled listing-weight shape + render (ws43-03) ───────────

    /// The weight-shape rule: a matched directory or several matched files is
    /// a listing (top-level weight by default); exactly one matched file is
    /// the file-outline shape (full tree by default).
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn plan_listing_shape_rule() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        std::fs::write(base.join("a.rs"), "fn a() {}\n").expect("write a");
        std::fs::write(base.join("b.rs"), "fn b() {}\n").expect("write b");
        let fs_manager = FilesystemManager::new();
        let plan = |pattern: PathBuf| {
            build_glob_plan(&fs_manager, &pattern, &ExcludeSet::default(), false, false)
                .expect("build plan")
        };

        assert!(
            plan(base.clone()).listing_shape,
            "a matched directory is a listing shape"
        );
        assert!(
            plan(base.join("*.rs")).listing_shape,
            "several matched files are a listing shape"
        );
        assert!(
            !plan(base.join("a.rs")).listing_shape,
            "a single matched file is the file-outline shape"
        );
    }

    /// The render honors the enrichment map: an outline body re-indents under
    /// its file header, a suppressed file carries `[symbols available]`, and a
    /// file with no annotation (every degrade arm) carries `no outline` — with
    /// the complete listing identical across all three.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn render_plan_honors_enrichment_map() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        let file = base.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write a");
        let fs_manager = FilesystemManager::new();
        let plan = build_glob_plan(&fs_manager, &base, &ExcludeSet::default(), false, false)
            .expect("build plan");
        assert_eq!(plan.enrich_files, vec![file.clone()], "one file to enrich");

        // Degrade (empty map): complete listing, `no outline` marker.
        let degraded = render_glob_plan(&plan, &HashMap::new());
        assert!(
            degraded.contains("a.rs  (1 line, no outline)"),
            "an unannotated text file carries the no-outline marker: {degraded}"
        );

        // An outline body re-indents one level under the entry header.
        let mut with_body = HashMap::new();
        with_body.insert(
            file.clone(),
            FileEnrichment {
                covered: true,
                suppressed: false,
                outline: Some("1  fn a() {\n\t2  fn nested()".to_string()),
            },
        );
        let outlined = render_glob_plan(&plan, &with_body);
        assert!(
            outlined.contains("\ta.rs  (1 line)\n\t\t1  fn a() {\n\t\t\t2  fn nested()\n"),
            "the body re-indents under the header, one level deeper: {outlined:?}"
        );

        // A suppressed outline keeps the `[symbols available]` flag in place
        // of the body.
        let mut suppressed = HashMap::new();
        suppressed.insert(
            file,
            FileEnrichment {
                covered: true,
                suppressed: true,
                outline: None,
            },
        );
        let flagged = render_glob_plan(&plan, &suppressed);
        assert!(
            flagged.contains("a.rs  (1 line) [symbols available]"),
            "a suppressed outline keeps the flag: {flagged}"
        );

        // The listing itself (header + entry line set) is identical across the
        // three arms — enrichment only ever adds indented body lines or
        // in-line markers, never drops a path (decision 025).
        for text in [&degraded, &outlined, &flagged] {
            assert!(
                text.contains(&format!("{}/  (1 file, 0 dirs)", base.display())),
                "the directory header always renders: {text}"
            );
            assert!(text.contains("a.rs"), "the entry always renders: {text}");
        }
    }

    /// A covered file with no symbols renders `no outline` too — a covered
    /// empty file and an uncovered one degrade to the same honest spelling.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn render_plan_covered_empty_is_no_outline() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        let file = base.join("a.rs");
        std::fs::write(&file, "// nothing\n").expect("write a");
        let fs_manager = FilesystemManager::new();
        let plan = build_glob_plan(&fs_manager, &base, &ExcludeSet::default(), false, false)
            .expect("build plan");

        let mut covered_empty = HashMap::new();
        covered_empty.insert(
            file,
            FileEnrichment {
                covered: true,
                suppressed: false,
                outline: None,
            },
        );
        let text = render_glob_plan(&plan, &covered_empty);
        assert!(
            text.contains("a.rs  (1 line, no outline)"),
            "covered-but-empty carries the same no-outline marker: {text}"
        );
    }

    // ─── directory walk cancellation ─────────────────────────────────

    /// A fired token quits the one-level enumeration at the first entry — the
    /// bounded-work property the cancel parameter keeps honest.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn collect_dir_entries_quits_on_cancel() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let dir = tmp.path().canonicalize().expect("canonicalize");
        for i in 0..200 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x\n").expect("write child");
        }

        let fs_manager = FilesystemManager::new();

        // Baseline: without cancellation the walk enumerates every child.
        let live = tokio_util::sync::CancellationToken::new();
        let full = collect_dir_entries(
            &fs_manager,
            &dir,
            false,
            false,
            &ExcludeSet::default(),
            &live,
        )
        .expect("walk uncancelled");
        assert_eq!(full.len(), 200, "uncancelled walk lists every child");

        // A token fired before the walk quits it at the first entry.
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let cancelled = collect_dir_entries(
            &fs_manager,
            &dir,
            false,
            false,
            &ExcludeSet::default(),
            &cancel,
        )
        .expect("walk cancelled");
        assert!(
            cancelled.is_empty(),
            "a fired token quits the directory walk (got {} entries)",
            cancelled.len(),
        );
    }

    // ─── expansion pathology tier (bug 78 / misc 159) ──────────────────
    //
    // A synthetic fixture of `k` sibling directories × `n` descendants,
    // asserting **operation counts, never wall clocks** (the bug-59 ruling —
    // time-based guards flake under contention, work-based guards don't). The
    // three contradictions the runaway's root cause had to explain map onto
    // these three tests: the base walk is not the cost (bounded expansion), the
    // count reads no content (zero bytes), and the returned set is complete.

    /// Builds the discriminating fixture: a base dir with `k` sibling
    /// directories, one of them (`big/`) carrying `n` flat text-file children,
    /// the others near-empty. Returns `(base, big)`, both canonicalized. This is
    /// the synthetic `~/.claude` analog — no dependence on any real state dir.
    #[allow(clippy::expect_used, reason = "test fixture construction")]
    fn pathology_fixture(k: usize, n: usize) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        // The one big sibling — `big/` — with `n` flat children.
        let big = base.join("big");
        std::fs::create_dir(&big).expect("mkdir big");
        for i in 0..n {
            std::fs::write(big.join(format!("f{i}.txt")), "line\n").expect("write child");
        }
        // `k - 1` near-empty siblings sharing the leading letter `b` so a
        // single-star `b*` resolves all of them at once (the multi-dir match the
        // runaway needed).
        for s in 0..k.saturating_sub(1) {
            let sib = base.join(format!("bit{s}"));
            std::fs::create_dir(&sib).expect("mkdir sibling");
            std::fs::write(sib.join("only.txt"), "x\n").expect("write sibling child");
        }
        (tmp, base, big)
    }

    /// Contradiction 1 — cost must track which dirs resolve, not the base walk.
    ///
    /// A single-star pattern (`/base/b*`) can only match `base`'s direct
    /// children, so the expansion walk must not descend into any sibling's
    /// subtree. With one sibling carrying `n` descendants, the pre-fix walk
    /// visited the whole tree (`> n` entries); the fix bounds it to the base's
    /// direct children. The assertion is independent of `n`: the same pattern
    /// against a 10× larger `big/` must walk the same handful of entries.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn pathology_expansion_walk_bounded_by_pattern_depth() {
        use tokio_util::sync::CancellationToken;

        let k = 6;
        let (_small_guard, small_base, _) = pathology_fixture(k, 100);
        let (_big_guard, big_base, _) = pathology_fixture(k, 5000);
        let fs_manager = FilesystemManager::new();
        let cancel = CancellationToken::new();

        // Small fixture: single-star `b*` expansion.
        probe::reset();
        let _ = count_glob_paths(
            &fs_manager,
            &[small_base.join("b*")],
            false,
            false,
            &ExcludeSet::default(),
            &cancel,
        );
        let small_expand = probe::expand_entries();

        // Big fixture (50× the descendants): same single-star `b*`.
        probe::reset();
        let _ = count_glob_paths(
            &fs_manager,
            &[big_base.join("b*")],
            false,
            false,
            &ExcludeSet::default(),
            &cancel,
        );
        let big_expand = probe::expand_entries();

        // The expansion walk is bounded by the pattern's depth (base + its
        // direct children), so it is the SAME for both fixtures — it does not
        // scale with the size of the matched sibling's subtree. `k` siblings +
        // `big/` + the base itself is `k + 1` entries; allow generous slack but
        // stay far below the descendant count.
        let bound = k + 2;
        assert!(
            small_expand <= bound,
            "single-star expansion must walk ≤ {bound} entries, walked {small_expand}"
        );
        assert_eq!(
            small_expand, big_expand,
            "expansion walk must not grow with the matched subtree size \
             (small={small_expand}, big={big_expand}) — it tracks pattern depth, \
             not descendant count"
        );
    }

    /// The free win — `--count` reads ZERO file content bytes and issues zero
    /// line-count reads (`file_info`), even when the pattern's match set is
    /// thousands of files. Under the one-verb form a directory pattern
    /// self-matches once (misc 184 — no enumeration of matched dirs at all), so
    /// this exercises the many-file leg with a `big/*` pattern that matches every
    /// child directly.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn pathology_count_reads_zero_content_bytes() {
        use crate::bridge::filesystem_manager::scan_bytes;
        use tokio_util::sync::CancellationToken;

        let n = 3000;
        let (_guard, base, _big) = pathology_fixture(3, n);
        let fs_manager = FilesystemManager::new();
        let cancel = CancellationToken::new();

        probe::reset();
        let count = count_glob_paths(
            &fs_manager,
            &[base.join("big/*")],
            false,
            false,
            &ExcludeSet::default(),
            &cancel,
        );

        assert_eq!(count, n, "sanity: big/*'s children are each counted once");
        assert_eq!(
            probe::file_info_calls(),
            0,
            "--count must never call file_info (line-count reads it throws away)"
        );
        assert_eq!(
            scan_bytes(),
            0,
            "--count must read zero file content bytes (read {} bytes)",
            scan_bytes()
        );
    }

    /// The returned set is exactly complete (count == the constructed N), and
    /// the total enumeration stays bounded by `c·N` — no superlinear blowup
    /// from combining a big matched directory with tiny siblings (the
    /// runaway's trigger). The pattern is `b*/*` — matching the files
    /// themselves — since a pattern's count is its match set (misc 184): `b*`
    /// would now count the `k` matched directories, not their children.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn pathology_count_complete_and_enumeration_bounded() {
        use tokio_util::sync::CancellationToken;

        let k = 5;
        let n = 2000;
        let (_guard, base, _big) = pathology_fixture(k, n);
        let fs_manager = FilesystemManager::new();
        let cancel = CancellationToken::new();

        // Constructed total the pattern `b*/*` matches: `big/`'s n children
        // plus `k - 1` siblings' one child each.
        let constructed = n + (k - 1);

        probe::reset();
        let count = count_glob_paths(
            &fs_manager,
            &[base.join("b*/*")],
            false,
            false,
            &ExcludeSet::default(),
            &cancel,
        );

        assert_eq!(
            count, constructed,
            "the returned count must be exactly the constructed N \
             (got {count}, expected {constructed}) — completeness, never partial"
        );

        // Total entries touched: under the one-verb form `--count` is a single
        // gitignore-aware expansion walk — no per-resolved-directory enumeration
        // (a matched directory counts once, never descended). The walk stays
        // `O(N)` — every matched path visited once — with no superlinear
        // interaction between the sibling count and the big directory's size.
        let touched = probe::expand_entries() + probe::collect_entries();
        let budget = 3 * constructed;
        assert!(
            touched <= budget,
            "expansion + enumeration must touch ≤ {budget} entries for N={constructed}, \
             touched {touched}"
        );
    }

    // ─── `--count` reports the match set (misc 184) ─────────────────────
    //
    // The sighting: `catenary glob 'tickets/*' --count` answered 753 for a
    // pattern that matched 45 subdirectories and 0 loose files. The count leg
    // descended into matched directories (tallying their entries) instead of
    // counting the match set. Under the VERBS one-verb form `--count` is the
    // *sole* tally (the divergent cardinality header retired), so the divergence
    // class is structurally impossible — these tests pin `count_paths` to an
    // independent match-set oracle over the ticket's probe fixture, keeping the
    // "count must not descend" tooth.

    /// Builds the misc-184 fixture mirroring the ticket's probe table:
    /// `tickets/` holds three subdirectories (two files each) and no loose
    /// files; `archive/` mixes two loose files with two subdirectories (one
    /// file each); `misc/` is a flat directory of four files.
    #[allow(clippy::expect_used, reason = "test fixture construction")]
    fn misc184_fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        for ws in ["ws1", "ws2", "ws3"] {
            let dir = base.join("tickets").join(ws);
            std::fs::create_dir_all(&dir).expect("mkdir workstream");
            for f in ["a.md", "b.md"] {
                std::fs::write(dir.join(f), "x\n").expect("write ticket");
            }
        }
        for d in ["old1", "old2"] {
            let dir = base.join("archive").join(d);
            std::fs::create_dir_all(&dir).expect("mkdir archive dir");
            std::fs::write(dir.join("kept.md"), "x\n").expect("write archived file");
        }
        for f in ["one.md", "two.md"] {
            std::fs::write(base.join("archive").join(f), "x\n").expect("write archive file");
        }
        let misc = base.join("misc");
        std::fs::create_dir(&misc).expect("mkdir misc");
        for i in 0..4 {
            std::fs::write(misc.join(format!("t{i}.md")), "x\n").expect("write misc file");
        }
        (tmp, base)
    }

    /// Runs [`count_glob_paths`] for a single pattern argument.
    fn misc184_count(fs_manager: &FilesystemManager, pattern: PathBuf) -> usize {
        count_glob_paths(
            fs_manager,
            &[pattern],
            false,
            false,
            &ExcludeSet::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
    }

    /// Independent match-set oracle: the number of paths the glob pattern
    /// resolves to, computed straight from the daemon-side expansion (not
    /// `count_paths`). This is the "one set" the ticket cares about — every
    /// match counted once, a matched directory *not* descended into. Comparing
    /// `count_paths` against it pins that the count leg reports the match set,
    /// never the descended contents (the misc-184 double-count).
    #[allow(clippy::expect_used, reason = "test helper")]
    fn misc184_match_set(pattern: &Path) -> usize {
        let cancel = tokio_util::sync::CancellationToken::new();
        let groups = crate::bridge::session::expand_glob_patterns_grouped_cancellable(
            std::slice::from_ref(&pattern.to_path_buf()),
            false,
            false,
            &cancel,
        );
        groups.iter().map(|g| g.resolved.len()).sum()
    }

    /// A pattern matching only directories counts the match set — one number —
    /// not the directories' contents. Also covers the ticket's mixed
    /// `archive/*` probe: loose files and subdirectories each count once. Under
    /// the one-verb form `--count` is the sole tally, so the retained tooth is
    /// `count_paths` == the independent match-set oracle (the header the old
    /// version compared against retired with the VERBS streams ruling).
    #[test]
    fn misc184_dir_match_count_equals_match_set() {
        let (_guard, base) = misc184_fixture();
        let fs_manager = FilesystemManager::new();

        // Directory-only match set: 3 dirs, 0 loose files — the sighting's
        // shape (45 dirs → count 753 pre-fix).
        let pattern = base.join("tickets/*");
        let count = misc184_count(&fs_manager, pattern.clone());
        assert_eq!(count, 3, "tickets/* matches 3 dirs — each counts once");
        assert_eq!(
            count,
            misc184_match_set(&pattern),
            "--count reports the match set, not the descended dir contents"
        );

        // Mixed match set (the archive probe): 2 files + 2 dirs = 4, not
        // 4 + the dirs' children.
        let pattern = base.join("archive/*");
        let count = misc184_count(&fs_manager, pattern.clone());
        assert_eq!(count, 4, "archive/* matches 2 files + 2 dirs — each once");
        assert_eq!(
            count,
            misc184_match_set(&pattern),
            "--count reports the match set, not the descended dir contents"
        );
    }

    /// A flat all-files pattern was correct before the fix and stays so — the
    /// common case that let the bug survive.
    #[test]
    fn misc184_flat_file_match_count_unchanged() {
        let (_guard, base) = misc184_fixture();
        let fs_manager = FilesystemManager::new();

        let pattern = base.join("misc/*");
        let count = misc184_count(&fs_manager, pattern.clone());
        assert_eq!(count, 4, "misc/* matches the 4 flat files");
        assert_eq!(
            count,
            misc184_match_set(&pattern),
            "--count reports the match set"
        );
    }

    /// Recursive `**/*` over a nested tree carries the same mechanism — dirs at
    /// depth ≥1 also match, and each counts once (the ticket's unverified probe,
    /// confirmed here).
    #[test]
    fn misc184_recursive_count_equals_match_set() {
        let (_guard, base) = misc184_fixture();
        let fs_manager = FilesystemManager::new();

        // tickets/**/* matches every path below tickets/: 3 dirs + 6 files.
        // Pre-fix each matched dir also contributed its children, double-
        // counting every file.
        let pattern = base.join("tickets/**/*");
        let count = misc184_count(&fs_manager, pattern.clone());
        assert_eq!(
            count, 9,
            "tickets/**/* matches 3 dirs + 6 files — each path once"
        );
        assert_eq!(
            count,
            misc184_match_set(&pattern),
            "--count reports the match set, not the descended dir contents"
        );
    }
}
