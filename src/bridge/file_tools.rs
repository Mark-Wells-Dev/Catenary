// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Glob tool handler: file/directory browsing.
//!
//! Each path is dispatched by type:
//! - File path → single file with defensive map (if LSP available)
//! - Directory path → listing with line counts, maps, and flags
//!
//! Output shape is determined by LSP coverage, not result volume:
//! - Enriched: file listing with defensive maps from symbol index (LSP available)
//! - Plain: file listing with entry flags (no LSP)
//!
//! Volume is bounded by the shared overflow valve (`overflow::valve`): the
//! display is truncated at a line budget — at a complete file boundary, so an
//! outline is never cut mid-tree — and the full output spills to a runtime-dir
//! file announced by a stderr receipt.

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::NO_LSP_LABEL;
use super::SourceLines;
use super::filesystem_manager::{
    FilesystemManager, STAT_RETRY_ATTEMPTS, format_file_size, mtime_nanos,
};
use super::overflow;
use super::session::{ResolvedGlob, expand_search_paths};
use crate::lsp::{LspClientManager, WalkBreadth};
use crate::symbol_index::{Symbol, SymbolIndex};

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// Literal file/directory paths (from shell expansion).
    ///
    /// Each path is dispatched through the appropriate handler
    /// (file outline, directory listing) without glob interpretation.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Glob pattern to exclude from results.
    #[serde(default)]
    pub exclude: Option<String>,
    /// Include gitignored files (default: false).
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files (default: false).
    #[serde(default)]
    pub include_hidden: bool,
    /// Working directory for cwd-scoped searches (relative patterns).
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Return a path count instead of rendered results (default: false).
    ///
    /// Short-circuits the overflow valve and LSP enrichment: the pipeline
    /// reports the number of resolved filesystem paths, never a rendered tree.
    #[serde(default)]
    pub count: bool,
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

/// Outcome of a glob query.
///
/// Normal queries render the full tree, bounded by the shared overflow valve;
/// `--count` (`GlobInput::count`) short-circuits to a path count.
pub enum GlobOutcome {
    /// Rendered tree output, truncated to the line budget (at a file boundary)
    /// by the overflow valve.
    Rendered {
        /// The (possibly truncated) output for stdout.
        output: String,
        /// A stderr receipt naming the full-output spill file, present only
        /// when the valve truncated the display.
        receipt: Option<String>,
    },
    /// `--count` summary: number of resolved filesystem paths.
    Count {
        /// Number of paths the query resolves to (files counted once each,
        /// directories counted by their listed entries).
        paths: usize,
    },
}

// ─── Glob tool server ─────────────────────────────────────────────────

/// Glob tool server: file/directory browsing.
pub struct GlobServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) symbol_index: Option<Arc<Mutex<SymbolIndex>>>,
    /// Line budget for the shared overflow valve (truncate-and-spill).
    pub(super) budget: usize,
    pub(super) outline_threshold: usize,
    pub(super) outline_suppress: Vec<globset::GlobMatcher>,
}

impl GlobServer {
    /// Execute a glob query with the given parameters.
    ///
    /// `parent_id` is a UUID for LSP event correlation — propagated to
    /// `ensure_symbols` so that `documentSymbol` traffic appears as
    /// children of this glob scope in the TUI.
    pub async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<&str>,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<GlobOutcome> {
        let input: GlobInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.paths.is_empty() {
            return Err(anyhow!("no paths provided"));
        }

        tracing::debug!("glob: {} path(s)", input.paths.len());

        // Compile exclude pattern via ResolvedGlob. The CLI router
        // resolves exclude against cwd before dispatch, so patterns
        // are always absolute here.
        let exclude = input
            .exclude
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(ResolvedGlob::new)
            .transpose()?;

        // Count mode short-circuits the overflow valve and enrichment — report
        // the number of resolved paths, not a rendered tree.
        if input.count {
            let paths = self.count_paths(&input, exclude.as_ref())?;
            return Ok(GlobOutcome::Count { paths });
        }

        // cwd-scoped search: present when the original pattern was relative.
        let cwd = input.cwd.as_deref();

        // Run pipeline — handlers return the complete output. Existing paths
        // dispatch directly; unexpanded glob patterns are expanded daemon-side.
        let full_output = self
            .handle_literal_paths(&input.paths, &input, exclude.as_ref(), cwd, parent_id)
            .await?;

        // Bound volume with the shared overflow valve: truncate the display at
        // the line budget and spill the full output to a runtime-dir file, with
        // the pointer carried out as a stderr receipt. Back the cut up to the
        // last complete file boundary so an outline is never severed mid-tree.
        let v = overflow::valve(
            &full_output,
            self.budget,
            &crate::paths::runtime_dir(),
            overflow::GLOB_PREFIX,
            Some(&is_outline_boundary),
        );
        Ok(GlobOutcome::Rendered {
            output: v.display,
            receipt: v.receipt,
        })
    }

    /// Single file: header with defensive map (if symbols available).
    ///
    /// Single files bypass `outline_threshold` — they get a map unless the
    /// grammar is not installed or the path matches `outline_suppress`.
    /// Returns the complete output.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    #[allow(
        clippy::option_if_let_else,
        reason = "side-effecting writeln in the Some branch"
    )]
    fn handle_glob_file(&self, path: &Path, cwd: Option<&Path>) -> String {
        let mut full = String::new();

        // Context header: `cwd: ~/…` for cwd-scoped, absolute path for absolute.
        let display = if let Some(cwd) = cwd {
            let compressed = super::compress_home(cwd);
            if self.fs_manager.resolve_root(cwd).is_some() {
                let _ = writeln!(full, "cwd: {compressed}");
            } else {
                let _ = writeln!(full, "cwd: {compressed} {NO_LSP_LABEL}");
            }
            path.strip_prefix(cwd).map_or_else(
                |_| path.to_string_lossy().to_string(),
                |rel| rel.to_string_lossy().to_string(),
            )
        } else {
            // Absolute pattern outside workspace roots: LSP warning.
            if self.fs_manager.resolve_root(path).is_none() {
                let _ = writeln!(full, "{NO_LSP_LABEL}");
            }
            path.to_string_lossy().to_string()
        };

        let metadata = std::fs::metadata(path).ok();

        // Detect snapshot or broken symlink.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_snapshot(&name) {
            let _ = writeln!(full, "{display} [snapshot]");
            return full;
        }

        // File header with line count or size.
        let line_count = metadata
            .as_ref()
            .and_then(|m| self.fs_manager.line_count(path, m));
        if let Some(lc) = line_count {
            let _ = writeln!(full, "{display}  ({lc} lines)");
        } else {
            let size = metadata.map_or(0, |m| m.len());
            let _ = writeln!(full, "{display}  ({})", format_file_size(size));
        }

        // Single-file map: bypass threshold, check symbols + deny only.
        let Some(ref ts_arc) = self.symbol_index else {
            return full;
        };
        let Ok(idx) = ts_arc.lock() else {
            return full;
        };
        if !idx.has_symbols_for(path)
            || is_outline_suppressed(path, &self.outline_suppress, &self.fs_manager)
        {
            return full;
        }

        let Ok(outline) = idx.query_outline_batch(&[path]) else {
            return full;
        };
        let Some(syms) = outline.get(path) else {
            return full;
        };

        // Build children set: names that appear as scope for other symbols.
        let children_set = idx
            .query(".*", Some(&[path.to_path_buf()]))
            .ok()
            .map(|all| {
                let mut cs = HashSet::new();
                for (_, s) in &all {
                    if let Some(ref scope) = s.scope {
                        cs.insert(scope.clone());
                    }
                }
                cs
            })
            .unwrap_or_default();

        let mut sources = SourceLines::new();
        for sym in syms {
            let source = sources.line(path, sym.line);
            render_symbol_line(&mut full, sym, Some(&children_set), "\t", source);
        }

        full
    }

    /// Dispatch each resolved path through the file or directory handler.
    ///
    /// Paths that exist are dispatched directly; non-existent paths are treated
    /// as glob patterns and expanded daemon-side via the gitignore-aware
    /// `ignore` walker ([`expand_search_paths`]). A shell-expanded (unquoted)
    /// glob arrives as concrete paths and an unexpanded (quoted) glob arrives
    /// as a pattern — both resolve to the same set here.
    async fn handle_literal_paths(
        &self,
        paths: &[PathBuf],
        input: &GlobInput,
        exclude: Option<&ResolvedGlob>,
        cwd: Option<&Path>,
        parent_id: Option<&str>,
    ) -> Result<String> {
        let resolved = expand_search_paths(paths, input.include_gitignored, input.include_hidden);

        // Scoped changed-set nudge (WS31 ticket 04): glob enriches with
        // `documentSymbol` (outlines) only, so coherence is needed just for the
        // files it lists — a `WalkBreadth::Scoped` walk of the glob pattern. Feed
        // the pattern's files into the ticket-03 diff (add/update only — a scoped
        // walk MUST NOT reap deletions, as it cannot assert a baseline entry
        // outside its pattern is gone), route the delta to covering servers, and
        // settle, BEFORE the outline queries below so they read the post-nudge
        // state. A root with no covering server is `WalkBreadth::None` and is
        // skipped. This runs before `ensure_symbols` for the same reason the
        // grep/diagnostics nudges precede their reads.
        self.nudge_scoped(&resolved, input, exclude).await;

        let mut full = String::new();
        for path in &resolved {
            // Directories first — `is_dir()` follows symlinks, so a
            // symlink-to-dir lists its contents (rather than rendering as a
            // single file header). This dir-first order matches
            // `collect_scoped_observations` so the listing and the changed-set
            // nudge classify a symlink-to-dir the same way (WS31-review walk-2).
            if path.is_dir() {
                let output = self
                    .handle_glob_dir(path, input, exclude, cwd, parent_id)
                    .await?;
                full.push_str(&output);
            } else if path_is_file_or_symlink_with_retry(path) {
                // Re-stat with a bounded retry: a transient
                // `is_file()`/`is_symlink()` miss (an atomic-rename write racing
                // this fresh stat) must not silently skip a named file that
                // `expand_search_paths` already confirmed present on disk.
                self.client_manager
                    .ensure_and_wait_for_paths(std::slice::from_ref(path))
                    .await;
                super::ensure_symbols(
                    self.symbol_index.as_ref(),
                    &self.client_manager,
                    &self.fs_manager,
                    std::slice::from_ref(path),
                    parent_id,
                )
                .await;
                full.push_str(&self.handle_glob_file(path, cwd));
            }
            // Skip non-existent paths silently — shell expansion
            // shouldn't produce them, but be defensive.
        }
        Ok(full)
    }

    /// Routes glob's scoped changed-set nudge (WS31 ticket 04,
    /// [`WalkBreadth::Scoped`](crate::lsp::WalkBreadth::Scoped)).
    ///
    /// `resolved` is the glob pattern's resolved path set (from
    /// [`expand_search_paths`]). The breadth of a glob walk is exactly the
    /// pattern, so the observation set is: each resolved file, and each resolved
    /// directory's **immediate** entries (the files glob lists) — the same
    /// visibility (`include_gitignored`/`include_hidden`) and `exclude` filters
    /// the listing applies, so the nudge tracks only files the query surfaces.
    /// Each observed file is statted (the per-file stat is the portable
    /// correctness path). The set is grouped by workspace root and routed via
    /// [`nudge_changed_set`](crate::lsp::LspClientManager::nudge_changed_set)
    /// with `reap = false`: a scoped walk adds/updates only and never reaps a
    /// deletion. Roots with no covering server are skipped
    /// ([`WalkBreadth::None`](crate::lsp::WalkBreadth::None)).
    async fn nudge_scoped(
        &self,
        resolved: &[PathBuf],
        input: &GlobInput,
        exclude: Option<&ResolvedGlob>,
    ) {
        let observations = collect_scoped_observations(resolved, input, exclude);
        if observations.is_empty() {
            return;
        }

        // Group by owning workspace root (root-relative path + mtime).
        let mut by_root: HashMap<PathBuf, Vec<(PathBuf, i64)>> = HashMap::new();
        for (abs, mtime) in observations {
            if let Some(root) = self.fs_manager.resolve_root(&abs)
                && let Ok(rel) = abs.strip_prefix(&root)
            {
                by_root
                    .entry(root)
                    .or_default()
                    .push((rel.to_path_buf(), mtime));
            }
        }

        let no_exclude: HashSet<PathBuf> = HashSet::new();
        for (root, observed) in &by_root {
            // Walk-breadth gate: a covered root is `Scoped` for glob, an
            // uncovered one is `None` (skip). A scoped walk never reaps.
            let breadth = if self.client_manager.has_covering_watchers(root).await {
                WalkBreadth::Scoped
            } else {
                WalkBreadth::None
            };
            if !breadth.runs_engine() {
                continue;
            }
            self.client_manager
                .nudge_changed_set(root, observed, &no_exclude, breadth.reaps())
                .await;
        }
    }

    /// Directory listing: enriched (maps) where LSP available, plain (flags) otherwise.
    ///
    /// Collects immediate children, applies visibility and exclude filters,
    /// detects flags (gitignored, snapshot, broken). Output shape is
    /// capability-driven, not volume-driven. Returns the complete output.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    async fn handle_glob_dir(
        &self,
        dir: &Path,
        input: &GlobInput,
        exclude: Option<&ResolvedGlob>,
        cwd: Option<&Path>,
        parent_id: Option<&str>,
    ) -> Result<String> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

        let entries = self.collect_dir_entries(&canonical, input, exclude)?;

        if entries.is_empty() {
            return Ok("Directory is empty".to_string());
        }

        // Populate symbol index for eligible files.
        let file_paths: Vec<PathBuf> = entries
            .iter()
            .filter(|e| !e.is_dir && !e.is_broken_symlink && !e.is_snapshot)
            .map(|e| e.abs_path.clone())
            .collect();
        self.client_manager
            .ensure_and_wait_for_paths(&file_paths)
            .await;
        super::ensure_symbols(
            self.symbol_index.as_ref(),
            &self.client_manager,
            &self.fs_manager,
            &file_paths,
            parent_id,
        )
        .await;

        let ts_guard = self.symbol_index.as_ref().and_then(|m| m.lock().ok());

        // Context header: `cwd: ~/…` for cwd-scoped, absolute path for absolute.
        let mut full = String::new();
        if let Some(cwd) = cwd {
            let compressed = super::compress_home(cwd);
            if self.fs_manager.resolve_root(cwd).is_some() {
                let _ = writeln!(full, "cwd: {compressed}");
            } else {
                let _ = writeln!(full, "cwd: {compressed} {NO_LSP_LABEL}");
            }
            let display = canonical.strip_prefix(cwd).map_or_else(
                |_| canonical.to_string_lossy().to_string(),
                |rel| rel.to_string_lossy().to_string(),
            );
            let _ = writeln!(full, "{display}/");
        } else {
            // Absolute pattern outside workspace roots: LSP warning.
            if self.fs_manager.resolve_root(&canonical).is_none() {
                let _ = writeln!(full, "{NO_LSP_LABEL}");
            }
            let _ = writeln!(full, "{}/", canonical.display());
        }

        // Render: enriched (maps) for eligible files, plain (flags) for the rest.
        let content = render_dir(
            &entries,
            ts_guard.as_deref(),
            self.outline_threshold,
            &self.outline_suppress,
            &self.fs_manager,
            "\t",
        );
        full.push_str(&content);
        Ok(full)
    }

    /// Extracts file info: `(line_count, binary_size)`.
    fn file_info(
        &self,
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> (Option<usize>, Option<String>) {
        metadata.map_or((None, None), |m| {
            self.fs_manager.line_count(path, m).map_or_else(
                || (None, Some(format_file_size(m.len()))),
                |lc| (Some(lc), None),
            )
        })
    }

    /// Collects the immediate children of a directory as `GlobEntry` rows.
    ///
    /// Applies the visibility (hidden), gitignore, and `exclude` filters and
    /// detects per-entry flags (gitignored, snapshot, symlink, broken). Shared
    /// by [`Self::handle_glob_dir`] (which renders the rows) and
    /// [`Self::count_paths`] (which counts them) so the two never diverge.
    /// `canonical` must be the canonicalized directory path.
    #[allow(clippy::too_many_lines, reason = "sequential per-entry classification")]
    fn collect_dir_entries(
        &self,
        canonical: &Path,
        input: &GlobInput,
        exclude: Option<&ResolvedGlob>,
    ) -> Result<Vec<GlobEntry>> {
        // Build non-gitignored set for flag detection.
        let non_ignored: HashSet<PathBuf> = if input.include_gitignored {
            WalkBuilder::new(canonical)
                .max_depth(Some(1))
                .git_ignore(true)
                .hidden(!input.include_hidden)
                .build()
                .flatten()
                .map(ignore::DirEntry::into_path)
                .collect()
        } else {
            HashSet::new()
        };

        let walker = WalkBuilder::new(canonical)
            .max_depth(Some(1))
            .git_ignore(!input.include_gitignored)
            .hidden(!input.include_hidden)
            .build();

        let mut entries = Vec::new();

        for entry in walker.flatten() {
            let entry_path = entry.into_path();
            if entry_path.as_path() == canonical {
                continue;
            }

            let name = entry_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            // Apply exclude filter against the entry path.
            if let Some(rg) = exclude
                && rg.is_match(&entry_path, canonical)
            {
                continue;
            }

            let is_gitignored = input.include_gitignored && !non_ignored.contains(&entry_path);
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
                    self.file_info(&entry_path, resolved_meta.as_ref())
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
                    self.file_info(&entry_path, Some(&metadata))
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

    /// Counts the filesystem paths a glob query resolves to (`--count`).
    ///
    /// Mirrors [`Self::handle_literal_paths`] dispatch — **directories first**,
    /// so a symlink-to-dir (which `is_dir()` follows) contributes its listed
    /// entry count, exactly as the listing renders it, rather than counting as a
    /// single file/symlink (WS31-review D1/T1). Each directory contributes the
    /// same filtered set [`Self::handle_glob_dir`] renders; each remaining
    /// resolved file or symlink-to-file counts once. LSP enrichment is skipped —
    /// a count is pure filesystem.
    fn count_paths(&self, input: &GlobInput, exclude: Option<&ResolvedGlob>) -> Result<usize> {
        let resolved =
            expand_search_paths(&input.paths, input.include_gitignored, input.include_hidden);
        let mut total = 0usize;
        for path in &resolved {
            if path.is_dir() {
                let canonical = path
                    .canonicalize()
                    .map_err(|e| anyhow!("Path does not exist: {}: {e}", path.display()))?;
                total += self.collect_dir_entries(&canonical, input, exclude)?.len();
            } else if path.is_file() || path.is_symlink() {
                total += 1;
            }
        }
        Ok(total)
    }
}

// ─── Map eligibility ──────────────────────────────────────────────────

/// Returns `true` if a file is eligible for defensive maps in a directory listing.
fn is_enrichment_eligible_entry(
    entry: &GlobEntry,
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    symbol_index: &SymbolIndex,
    fs_manager: &FilesystemManager,
) -> bool {
    !entry.is_dir
        && !entry.is_broken_symlink
        && !entry.is_snapshot
        && entry.line_count.is_some_and(|lc| lc >= outline_threshold)
        && symbol_index.has_symbols_for(&entry.abs_path)
        && !is_outline_suppressed(&entry.abs_path, outline_suppress, fs_manager)
}

/// Returns `true` if symbols are cached for the file in the index.
fn has_symbols_available(path: &Path, symbol_index: Option<&SymbolIndex>) -> bool {
    symbol_index.is_some_and(|idx| idx.has_symbols_for(path))
}

/// Returns `true` if the file matches any `outline_suppress` pattern.
fn is_outline_suppressed(
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

// ─── Symbol rendering ─────────────────────────────────────────────────

/// Whether a glob output line *begins* a complete top-level block, i.e. is a
/// safe place for the overflow valve to truncate.
///
/// A symbol-detail row renders `{indent}:{line}  {decl}` (see
/// [`render_symbol_line`]), so any line that does **not** start with `:` (after
/// its indentation) is a file or directory header — the start of a fresh
/// outline. Cutting there never severs a file's outline mid-tree. This module
/// owns the glob line format, so it owns the boundary predicate the shared
/// valve calls back into.
fn is_outline_boundary(line: &str) -> bool {
    !line.trim_start().starts_with(':')
}

/// Renders a single outline node as the one atom: `:line  <source line>[/]`.
///
/// The declaration line at the symbol's range start carries the kind
/// implicitly (`fn foo(...)`, `struct Bar`, `# Heading`), so no `<Kind>`
/// label is rendered (decision 024). When the source line is unavailable
/// (file unreadable or line out of range) the bare name is used as a fall
/// back so the node is never empty. The trailing `/` (has-children)
/// indicator — a structural signal, not a kind label — is kept.
fn render_symbol_line(
    out: &mut String,
    sym: &Symbol,
    children_set: Option<&HashSet<String>>,
    indent: &str,
    source: Option<&str>,
) {
    let trailing = if children_set.is_some_and(|cs| cs.contains(&sym.name)) {
        "/"
    } else {
        ""
    };
    let text = source.map_or_else(|| sym.name.as_str(), str::trim_end);
    let _ = writeln!(out, "{indent}:{}  {text}{trailing}", sym.line + 1);
}

/// Renders one file's outline as one atom per node.
///
/// Each node is its declaration source line, indented under the file
/// header (decision 024: the structure is the indentation, not a kind
/// label). The one-atom model retired the cross-file "common structure"
/// collapse — an outline shows each symbol once in its structural slot,
/// nothing recurs (collapse is grep-only). The file is read once via
/// [`SourceLines`] and indexed by line number for each node's source text.
fn render_individual_map(
    out: &mut String,
    file: &Path,
    syms: &[Symbol],
    children_set: Option<&HashSet<String>>,
    sym_indent: &str,
    sources: &mut SourceLines,
) {
    for sym in syms {
        let source = sources.line(file, sym.line);
        render_symbol_line(out, sym, children_set, sym_indent, source);
    }
}

// ─── Directory rendering ─────────────────────────────────────────────

/// Renders a directory listing: outline maps for files with LSP symbols,
/// plain (flags) for the rest.
///
/// Each map-eligible file renders its own outline — one atom per node, the
/// declaration source line indented under the file header (decision 024).
/// The cross-file "common structure" collapse is retired: an outline shows
/// each symbol once in its structural slot, nothing recurs.
#[allow(clippy::too_many_lines, reason = "sequential rendering pipeline")]
fn render_dir(
    entries: &[GlobEntry],
    symbol_index: Option<&SymbolIndex>,
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
    indent: &str,
) -> String {
    let sym_indent = format!("{indent}\t");
    let Some(idx) = symbol_index else {
        // No symbol index — render everything as plain (flags).
        return render_dir_plain(
            entries,
            None,
            outline_threshold,
            outline_suppress,
            fs_manager,
            indent,
        );
    };

    let eligible_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            is_enrichment_eligible_entry(e, outline_threshold, outline_suppress, idx, fs_manager)
        })
        .map(|(i, _)| i)
        .collect();

    if eligible_indices.is_empty() {
        return render_dir_plain(
            entries,
            Some(idx),
            outline_threshold,
            outline_suppress,
            fs_manager,
            indent,
        );
    }

    let eligible_refs: Vec<&Path> = eligible_indices
        .iter()
        .map(|&i| entries[i].abs_path.as_path())
        .collect();
    let Ok(outline) = idx.query_outline_batch(&eligible_refs) else {
        return render_dir_plain(
            entries,
            Some(idx),
            outline_threshold,
            outline_suppress,
            fs_manager,
            indent,
        );
    };
    if outline.is_empty() {
        return render_dir_plain(
            entries,
            Some(idx),
            outline_threshold,
            outline_suppress,
            fs_manager,
            indent,
        );
    }

    // Build children sets for the has-children (`/`) indicator.
    let children_sets = build_children_sets(idx, &eligible_refs);
    let eligible_set: HashSet<usize> = eligible_indices.iter().copied().collect();
    let mut sources = SourceLines::new();
    let mut result = String::new();

    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    for d in &dirs {
        let _ = writeln!(result, "{indent}{}", d.name);
    }

    let mut files: Vec<(usize, &GlobEntry)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_dir)
        .collect();
    files.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    for &(ei, f) in &files {
        if eligible_set.contains(&ei)
            && let Some(syms) = outline.get(&f.abs_path)
        {
            let flags = compute_entry_flags(f, Some(idx), 0, outline_suppress, fs_manager, true);
            render_entry_line(&mut result, f, &flags, indent);
            let cs = children_sets.get(&f.abs_path);
            render_individual_map(
                &mut result,
                &f.abs_path,
                syms,
                cs,
                &sym_indent,
                &mut sources,
            );
        } else {
            // Non-eligible file: plain flags.
            let flags = compute_entry_flags(
                f,
                Some(idx),
                outline_threshold,
                outline_suppress,
                fs_manager,
                false,
            );
            render_entry_line(&mut result, f, &flags, indent);
        }
    }

    result
}

/// Renders a flat directory listing with entry flags (plain).
fn render_dir_plain(
    entries: &[GlobEntry],
    symbol_index: Option<&SymbolIndex>,
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
    indent: &str,
) -> String {
    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    let mut files: Vec<&GlobEntry> = entries.iter().filter(|e| !e.is_dir).collect();

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let mut result = String::new();
    for d in &dirs {
        let _ = writeln!(result, "{indent}{}", d.name);
    }
    for f in &files {
        let flags = compute_entry_flags(
            f,
            symbol_index,
            outline_threshold,
            outline_suppress,
            fs_manager,
            false,
        );
        render_entry_line(&mut result, f, &flags, indent);
    }
    result
}

/// Computes entry flags for a `GlobEntry`.
fn compute_entry_flags<'a>(
    entry: &GlobEntry,
    symbol_index: Option<&SymbolIndex>,
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
    map_rendered: bool,
) -> Vec<&'a str> {
    let mut flags = Vec::new();

    if entry.is_broken_symlink {
        flags.push("broken");
        return flags;
    }

    if entry.is_snapshot {
        flags.push("snapshot");
        return flags;
    }

    if !map_rendered
        && has_symbols_available(&entry.abs_path, symbol_index)
        && entry.line_count.is_some_and(|lc| lc >= outline_threshold)
    {
        flags.push("symbols available");
    }

    if map_rendered
        && has_symbols_available(&entry.abs_path, symbol_index)
        && is_outline_suppressed(&entry.abs_path, outline_suppress, fs_manager)
    {
        flags.push("symbols available");
    }

    if entry.is_gitignored {
        flags.push("gitignored");
    }

    flags
}

/// Renders a `GlobEntry` line with flags.
fn render_entry_line(out: &mut String, entry: &GlobEntry, flags: &[&str], indent: &str) {
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
        if let Some(lc) = entry.line_count {
            let _ = writeln!(
                out,
                "{indent}{} -> {target}  ({lc} lines){flag_str}",
                entry.name
            );
        } else if let Some(ref size) = entry.binary_size {
            let _ = writeln!(
                out,
                "{indent}{} -> {target}  ({size}){flag_str}",
                entry.name
            );
        } else {
            let _ = writeln!(out, "{indent}{} -> {target}{flag_str}", entry.name);
        }
    } else if let Some(ref size) = entry.binary_size {
        let _ = writeln!(out, "{indent}{}  ({size}){flag_str}", entry.name);
    } else if let Some(lc) = entry.line_count {
        let _ = writeln!(out, "{indent}{}  ({lc} lines){flag_str}", entry.name);
    } else {
        let _ = writeln!(out, "{indent}{}{flag_str}", entry.name);
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

/// Builds per-file children sets from the tree-sitter index.
///
/// For each file, collects the set of symbol names that are used as
/// `scope` by other symbols — these are containers that get trailing `/`.
fn build_children_sets(
    symbol_index: &SymbolIndex,
    files: &[&Path],
) -> HashMap<PathBuf, HashSet<String>> {
    let mut result = HashMap::new();
    for &path in files {
        let mut cs = HashSet::new();
        if let Ok(all) = symbol_index.query(".*", Some(&[path.to_path_buf()])) {
            for (_, s) in &all {
                if let Some(ref scope) = s.scope {
                    cs.insert(scope.clone());
                }
            }
        }
        if !cs.is_empty() {
            result.insert(path.to_path_buf(), cs);
        }
    }
    result
}

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
    input: &GlobInput,
    exclude: Option<&ResolvedGlob>,
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
                .git_ignore(!input.include_gitignored)
                .hidden(!input.include_hidden)
                .build();
            for entry in walker.flatten() {
                let entry_is_symlink = entry.path_is_symlink();
                let entry_path = entry.into_path();
                if entry_path.as_path() == path.as_path() {
                    continue;
                }
                if let Some(rg) = exclude
                    && rg.is_match(&entry_path, path)
                {
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

#[cfg(test)]
mod tests {
    use super::*;
    use globset::Glob;

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

    // ─── is_enrichment_eligible_entry ────────────────────────────────

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

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_all_conditions() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/eligible.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "foo",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let entry = make_glob_entry("eligible.rs", &path, false, Some(200));
        let fs = FilesystemManager::new();

        // All conditions met → eligible.
        assert!(
            is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "should be eligible when all conditions met"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_dir_excluded() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/dir");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let entry = make_glob_entry("dir", &path, true, Some(200));
        let fs = FilesystemManager::new();

        assert!(
            !is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "directories should not be enrichment eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_broken_symlink_excluded() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/broken.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let mut entry = make_glob_entry("broken.rs", &path, false, Some(200));
        entry.is_broken_symlink = true;
        let fs = FilesystemManager::new();

        assert!(
            !is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "broken symlinks should not be enrichment eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_snapshot_excluded() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/snap.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let mut entry = make_glob_entry("snap.rs", &path, false, Some(200));
        entry.is_snapshot = true;
        let fs = FilesystemManager::new();

        assert!(
            !is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "snapshot files should not be enrichment eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_below_threshold() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/small.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        // line_count (50) < threshold (100) → not eligible
        let entry = make_glob_entry("small.rs", &path, false, Some(50));
        let fs = FilesystemManager::new();

        assert!(
            !is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "file below threshold should not be enrichment eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_at_threshold_boundary() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/boundary.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        // line_count == threshold → eligible (>= check)
        let entry = make_glob_entry("boundary.rs", &path, false, Some(100));
        let fs = FilesystemManager::new();

        assert!(
            is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "file at exact threshold should be eligible"
        );

        // line_count one below threshold → not eligible
        let entry_below = make_glob_entry("boundary.rs", &path, false, Some(99));
        assert!(
            !is_enrichment_eligible_entry(&entry_below, 100, &[], &idx, &fs),
            "file one below threshold should not be eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_no_symbols() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/no_syms.rs");
        // Don't populate any symbols for this path.

        let entry = make_glob_entry("no_syms.rs", &path, false, Some(200));
        let fs = FilesystemManager::new();

        assert!(
            !is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "file without symbols should not be enrichment eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_suppressed() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/suppressed.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let entry = make_glob_entry("suppressed.rs", &path, false, Some(200));
        let suppress = vec![
            Glob::new("**/*.rs")
                .expect("compile glob")
                .compile_matcher(),
        ];
        let fs = FilesystemManager::new();

        assert!(
            !is_enrichment_eligible_entry(&entry, 100, &suppress, &idx, &fs),
            "suppressed file should not be enrichment eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_entry_no_line_count() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/no_lc.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        // line_count = None → is_some_and fails → not eligible
        let entry = make_glob_entry("no_lc.rs", &path, false, None);
        let fs = FilesystemManager::new();

        assert!(
            !is_enrichment_eligible_entry(&entry, 100, &[], &idx, &fs),
            "file with no line count should not be enrichment eligible"
        );
    }

    // ─── has_symbols_available ───────────────────────────────────────

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_has_symbols_available_with_symbols() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/has_syms.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "foo",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        assert!(
            has_symbols_available(&path, Some(&idx)),
            "should return true when symbols exist"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_has_symbols_available_without_symbols() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/no_syms.rs");

        assert!(
            !has_symbols_available(&path, Some(&idx)),
            "should return false when no symbols"
        );
    }

    #[test]
    fn test_has_symbols_available_no_index() {
        let path = PathBuf::from("/test/any.rs");

        assert!(
            !has_symbols_available(&path, None),
            "should return false with no index"
        );
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

    // ─── render_symbol_line (one-atom: source line, no kind) ────────

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
        // The atom is the declaration source line; the kind is implicit in it.
        render_symbol_line(
            &mut out,
            &sym,
            None,
            "\t",
            Some("fn my_func(x: u32) -> u32 {"),
        );

        assert_eq!(out, "\t:10  fn my_func(x: u32) -> u32 {\n");
    }

    #[test]
    fn test_render_symbol_line_name_embedding_no_double_prefix() {
        // A name-embedding server (lattice `H1:`) — the heading source line is
        // already clean, so there is no `<Class>` to double-prefix.
        let sym = Symbol {
            name: "H1: My Heading".to_string(),
            kind: "class".to_string(),
            line: 0,
            end_line: 1,
            scope: None,
            scope_kind: None,
            deprecated: false,
        };

        let mut out = String::new();
        render_symbol_line(&mut out, &sym, None, "\t", Some("# My Heading"));

        assert_eq!(out, "\t:1  # My Heading\n");
        assert!(!out.contains('<'), "no kind label: {out:?}");
    }

    #[test]
    fn test_render_symbol_line_keeps_children_slash() {
        let sym = Symbol {
            name: "H1: Parent".to_string(),
            kind: "class".to_string(),
            line: 0,
            end_line: 9,
            scope: None,
            scope_kind: None,
            deprecated: true,
        };

        let children: HashSet<String> = ["H1: Parent".to_string()].into();
        let mut out = String::new();
        render_symbol_line(&mut out, &sym, Some(&children), "\t", Some("# Parent"));

        // Source line carries the structure; trailing `/` (has-children) kept.
        assert_eq!(out, "\t:1  # Parent/\n");
    }

    #[test]
    fn test_render_symbol_line_with_children() {
        let sym = Symbol {
            name: "MyStruct".to_string(),
            kind: "struct".to_string(),
            line: 0,
            end_line: 10,
            scope: None,
            scope_kind: None,
            deprecated: false,
        };

        let children: HashSet<String> = ["MyStruct".to_string()].into();
        let mut out = String::new();
        render_symbol_line(
            &mut out,
            &sym,
            Some(&children),
            "\t",
            Some("struct MyStruct {"),
        );

        assert_eq!(out, "\t:1  struct MyStruct {/\n");
    }

    #[test]
    fn test_render_symbol_line_trailing_whitespace_trimmed() {
        // Trailing whitespace on the source line is trimmed so the `/`
        // indicator (and byte-stable output) stays adjacent to the text.
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
        render_symbol_line(&mut out, &sym, None, "", Some("fn old_fn() {   "));

        assert_eq!(out, ":5  fn old_fn() {\n");
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
        render_symbol_line(&mut out, &sym, None, "", None);

        assert_eq!(out, ":1  standalone\n");
    }

    // ─── render_individual_map (per-file outline, one atom per node) ─

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_render_individual_map_reads_declaration_lines() {
        // The outline renders each node's declaration source line, nested by
        // indentation — no `<Kind>` label, no "common structure" collapse.
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("code.rs");
        std::fs::write(&file, "fn alpha() {}\nstruct Bar {\n    x: u32,\n}\n").expect("write file");

        let syms = vec![
            Symbol {
                name: "alpha".to_string(),
                kind: "function".to_string(),
                line: 0,
                end_line: 0,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
            Symbol {
                name: "Bar".to_string(),
                kind: "struct".to_string(),
                line: 1,
                end_line: 3,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
        ];
        // `Bar` has a field child → trailing `/`.
        let children: HashSet<String> = ["Bar".to_string()].into();

        let mut out = String::new();
        let mut sources = SourceLines::new();
        render_individual_map(&mut out, &file, &syms, Some(&children), "\t", &mut sources);

        assert_eq!(out, "\t:1  fn alpha() {}\n\t:2  struct Bar {/\n");
        assert!(!out.contains('<'), "no kind label anywhere: {out:?}");
    }

    // ─── compute_entry_flags ───────────────────────────────────────

    #[test]
    fn test_compute_entry_flags_broken_symlink() {
        let mut entry = make_glob_entry("link.rs", Path::new("/test/link.rs"), false, Some(100));
        entry.is_broken_symlink = true;
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, None, 100, &[], &fs, false);
        assert_eq!(
            flags,
            vec!["broken"],
            "broken symlink should return [broken]"
        );
    }

    #[test]
    fn test_compute_entry_flags_snapshot() {
        let mut entry = make_glob_entry("snap.rs", Path::new("/test/snap.rs"), false, Some(200));
        entry.is_snapshot = true;
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, None, 100, &[], &fs, false);
        assert_eq!(flags, vec!["snapshot"], "snapshot should return [snapshot]");
    }

    #[test]
    fn test_compute_entry_flags_empty_when_no_conditions() {
        let entry = make_glob_entry("plain.rs", Path::new("/test/plain.rs"), false, Some(50));
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, None, 100, &[], &fs, false);
        assert!(flags.is_empty(), "no conditions met → empty: {flags:?}");
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_entry_flags_symbols_available_not_rendered() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/big.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let entry = make_glob_entry("big.rs", &path, false, Some(200));
        let fs = FilesystemManager::new();

        // !map_rendered + has symbols + above threshold → symbols available.
        let flags = compute_entry_flags(&entry, Some(&idx), 100, &[], &fs, false);
        assert_eq!(
            flags,
            vec!["symbols available"],
            "above threshold with symbols should flag: {flags:?}"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_entry_flags_symbols_available_rendered_suppressed() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/suppressed.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let entry = make_glob_entry("suppressed.rs", &path, false, Some(200));
        let suppress = vec![
            Glob::new("**/*.rs")
                .expect("compile glob")
                .compile_matcher(),
        ];
        let fs = FilesystemManager::new();

        // map_rendered + has symbols + suppressed → symbols available.
        let flags = compute_entry_flags(&entry, Some(&idx), 100, &suppress, &fs, true);
        assert_eq!(
            flags,
            vec!["symbols available"],
            "map_rendered + suppressed should flag: {flags:?}"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_entry_flags_not_rendered_below_threshold() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/small.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        // Below threshold, not rendered → no symbols available flag.
        let entry = make_glob_entry("small.rs", &path, false, Some(50));
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, Some(&idx), 100, &[], &fs, false);
        assert!(
            flags.is_empty(),
            "below threshold should not flag: {flags:?}"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_entry_flags_rendered_not_suppressed() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/rendered.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        // map_rendered but NOT suppressed → no flag.
        let entry = make_glob_entry("rendered.rs", &path, false, Some(200));
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, Some(&idx), 100, &[], &fs, true);
        assert!(
            flags.is_empty(),
            "map_rendered without suppress should not flag: {flags:?}"
        );
    }

    #[test]
    fn test_compute_entry_flags_gitignored() {
        let mut entry = make_glob_entry("debug.log", Path::new("/test/debug.log"), false, Some(10));
        entry.is_gitignored = true;
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, None, 200, &[], &fs, false);
        assert_eq!(flags, vec!["gitignored"]);
    }

    #[test]
    fn test_compute_entry_flags_broken_early_return() {
        // Broken symlink returns ["broken"] even if gitignored or snapshot.
        let mut entry = make_glob_entry("link.rs", Path::new("/test/link.rs"), false, Some(100));
        entry.is_broken_symlink = true;
        entry.is_gitignored = true;
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, None, 100, &[], &fs, false);
        assert_eq!(
            flags,
            vec!["broken"],
            "broken early return should not include gitignored: {flags:?}"
        );
    }

    #[test]
    fn test_compute_entry_flags_snapshot_early_return() {
        // Snapshot returns ["snapshot"] even if gitignored.
        let mut entry = make_glob_entry("snap.rs", Path::new("/test/snap.rs"), false, Some(200));
        entry.is_snapshot = true;
        entry.is_gitignored = true;
        let fs = FilesystemManager::new();

        let flags = compute_entry_flags(&entry, None, 100, &[], &fs, false);
        assert_eq!(
            flags,
            vec!["snapshot"],
            "snapshot early return should not include gitignored: {flags:?}"
        );
    }

    // ─── render_entry_line ─────────────────────────────────────────

    #[test]
    fn test_render_entry_line_regular_file_with_lines() {
        let entry = make_glob_entry("main.rs", Path::new("/test/main.rs"), false, Some(42));

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &[], "");

        assert_eq!(out, "main.rs  (42 lines)\n");
    }

    #[test]
    fn test_render_entry_line_with_flags() {
        let entry = make_glob_entry("main.rs", Path::new("/test/main.rs"), false, Some(42));

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &["symbols available"], "");

        assert_eq!(out, "main.rs  (42 lines) [symbols available]\n");
    }

    #[test]
    fn test_render_entry_line_broken_symlink() {
        let mut entry = make_glob_entry("broken.rs", Path::new("/test/broken.rs"), false, Some(10));
        entry.is_broken_symlink = true;
        entry.symlink_target = Some("/nonexistent".to_string());

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &["broken"], "");

        assert_eq!(out, "broken.rs -> /nonexistent [broken]\n");
    }

    #[test]
    fn test_render_entry_line_snapshot() {
        let mut entry = make_glob_entry("snap.rs", Path::new("/test/snap.rs"), false, Some(10));
        entry.is_snapshot = true;

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &["snapshot"], "");

        assert_eq!(out, "snap.rs [snapshot]\n");
    }

    #[test]
    fn test_render_entry_line_symlink_with_lines() {
        let mut entry = make_glob_entry("link.rs", Path::new("/test/link.rs"), false, Some(50));
        entry.is_symlink = true;
        entry.symlink_target = Some("/real/file.rs".to_string());

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &[], "");

        assert_eq!(out, "link.rs -> /real/file.rs  (50 lines)\n");
    }

    #[test]
    fn test_render_entry_line_binary() {
        let mut entry = make_glob_entry("data.bin", Path::new("/test/data.bin"), false, None);
        entry.binary_size = Some("1.5 MB".to_string());

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &[], "");

        assert_eq!(out, "data.bin  (1.5 MB)\n");
    }

    #[test]
    fn test_render_entry_line_no_size_no_lines() {
        let entry = make_glob_entry("empty", Path::new("/test/empty"), false, None);

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &[], "");

        assert_eq!(out, "empty\n");
    }

    #[test]
    fn test_render_entry_line_indented() {
        let entry = make_glob_entry("nested.rs", Path::new("/test/nested.rs"), false, Some(10));

        let mut out = String::new();
        render_entry_line(&mut out, &entry, &[], "\t");

        assert_eq!(out, "\tnested.rs  (10 lines)\n");
    }

    // ─── build_children_sets ───────────────────────────────────────

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_build_children_sets_with_scoped_symbols() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/has_scope.rs");

        // Symbol "method" with scope "MyStruct".
        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([
                {
                    "name": "MyStruct",
                    "kind": 23,
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 20, "character": 1 } },
                    "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 15 } },
                    "children": [{
                        "name": "method",
                        "kind": 12,
                        "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 5, "character": 5 } },
                        "selectionRange": { "start": { "line": 2, "character": 7 }, "end": { "line": 2, "character": 13 } }
                    }]
                }
            ]),
        )
        .expect("populate");

        let files: Vec<&Path> = vec![path.as_path()];
        let result = build_children_sets(&idx, &files);

        assert!(
            result.contains_key(&path),
            "should have entry for file with scoped symbols"
        );
        let cs = &result[&path];
        assert!(
            cs.contains("MyStruct"),
            "children set should contain the scope name: {cs:?}"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_build_children_sets_no_scoped_symbols() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/no_scope.rs");

        // Top-level symbol with no children/scope.
        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "standalone",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 13 } }
            }]),
        )
        .expect("populate");

        let files: Vec<&Path> = vec![path.as_path()];
        let result = build_children_sets(&idx, &files);

        assert!(
            !result.contains_key(&path),
            "file without scoped symbols should not appear in result"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_build_children_sets_multiple_files() {
        let idx = SymbolIndex::new().expect("create index");

        let path_a = PathBuf::from("/test/a.rs");
        let path_b = PathBuf::from("/test/b.rs");

        // File A has scoped symbol.
        idx.populate_from_document_symbols(
            &path_a,
            &serde_json::json!([
                {
                    "name": "Container",
                    "kind": 23,
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 10, "character": 1 } },
                    "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 16 } },
                    "children": [{
                        "name": "inner",
                        "kind": 12,
                        "range": { "start": { "line": 1, "character": 4 }, "end": { "line": 3, "character": 5 } },
                        "selectionRange": { "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 12 } }
                    }]
                }
            ]),
        )
        .expect("populate a");

        // File B has no scoped symbols.
        idx.populate_from_document_symbols(
            &path_b,
            &serde_json::json!([{
                "name": "top",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate b");

        let files: Vec<&Path> = vec![path_a.as_path(), path_b.as_path()];
        let result = build_children_sets(&idx, &files);

        assert!(
            result.contains_key(&path_a),
            "file A with scoped symbols should be in result"
        );
        assert!(
            !result.contains_key(&path_b),
            "file B without scoped symbols should not be in result"
        );
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
        let input: GlobInput = serde_json::from_value(serde_json::json!({
            "include_hidden": true,
            "include_gitignored": true,
        }))
        .expect("deserialize GlobInput");

        let observed = collect_scoped_observations(std::slice::from_ref(&linkdir), &input, None);

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

    // ─── count_paths — dispatch parity with handle_literal_paths (WS31-review D1) ──

    /// Builds a minimal [`GlobServer`] for unit tests. No LSP servers are
    /// spawned — [`LspClientManager::new`] only stores the config/logging/fs
    /// ports — so this is cheap and exercises the real filesystem dispatch in
    /// [`GlobServer::count_paths`] / [`GlobServer::collect_dir_entries`].
    fn test_glob_server() -> GlobServer {
        use crate::config::Config;
        use crate::logging::LoggingServer;
        use crate::lsp::LspClientManager;

        let fs_manager = Arc::new(FilesystemManager::new());
        let client_manager = Arc::new(LspClientManager::new(
            Config::default(),
            LoggingServer::new(),
            fs_manager.clone(),
        ));
        GlobServer {
            client_manager,
            fs_manager,
            symbol_index: None,
            budget: 2000,
            outline_threshold: 200,
            outline_suppress: vec![],
        }
    }

    /// T1 — `count_paths` must classify a symlink-to-dir the same way the listing
    /// (`handle_literal_paths`, dir-first since the C1 walk-2 reorder) does:
    /// follow the link and count the directory's listed entries (`N`), not treat
    /// it as a single file/symlink (`1`). Pre-fix (file/symlink branch first) the
    /// symlink-to-dir hits the `is_symlink()` branch → count `1` while the
    /// listing renders `N` — `glob count:true` desyncs from the listing.
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

        let server = test_glob_server();
        let input: GlobInput = serde_json::from_value(serde_json::json!({
            "paths": [linkdir.to_string_lossy()],
            "count": true,
        }))
        .expect("deserialize GlobInput");

        let count = server.count_paths(&input, None).expect("count_paths");

        assert_eq!(
            count, N,
            "count for a symlink-to-dir arg must equal the listing entry count \
             (N={N}, the dir's files), not 1 (the symlink-as-file count); got {count}"
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
}
