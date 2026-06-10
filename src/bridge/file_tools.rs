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
//! When results exceed the budget, output is paged via the `page` parameter.

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::filesystem_manager::{FilesystemManager, format_file_size};
use super::session::{ResolvedGlob, expand_search_paths};
use crate::lsp::LspClientManager;
use crate::symbol_index::{Symbol, SymbolIndex, format_symbol_kind};

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
    /// Page number for paged results (1-based, default: 1).
    #[serde(default = "default_page")]
    pub page: usize,
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
    /// Short-circuits pagination and LSP enrichment: the pipeline reports the
    /// number of resolved filesystem paths, never a page.
    #[serde(default)]
    pub count: bool,
}

/// Default page number (1-based).
const fn default_page() -> usize {
    1
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
/// Normal queries render a paginated tree; `--count` (`GlobInput::count`)
/// short-circuits to a path count instead of a page.
pub enum GlobOutcome {
    /// Rendered, paginated tree output.
    Rendered(String),
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
    pub(super) budget: usize,
    pub(super) outline_threshold: usize,
    pub(super) outline_suppress: Vec<globset::GlobMatcher>,
    /// Single-slot result cache for sequential page fetches.
    pub(super) cache: std::sync::Mutex<super::result_cache::ResultCache>,
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
        use super::pagination::paginate;
        use super::result_cache::{GlobCacheParams, cache_key};

        let input: GlobInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.paths.is_empty() {
            return Err(anyhow!("no paths provided"));
        }

        tracing::debug!("glob: {} path(s)", input.paths.len());

        let page = input.page.max(1);

        // Compute cache key from pipeline-affecting parameters.
        let key = cache_key(&GlobCacheParams {
            paths: &input.paths,
            exclude: input.exclude.as_deref(),
            include_gitignored: input.include_gitignored,
            include_hidden: input.include_hidden,
            cwd: input.cwd.as_deref(),
            budget: self.budget,
        });

        // Check cache before running the pipeline. Count queries bypass it:
        // the cache stores rendered pages, and a count is a different shape.
        if !input.count
            && let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(key, page, &self.fs_manager)
        {
            return Ok(GlobOutcome::Rendered(cached));
        }

        // Compile exclude pattern via ResolvedGlob. The CLI router
        // resolves exclude against cwd before dispatch, so patterns
        // are always absolute here.
        let exclude = input
            .exclude
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(ResolvedGlob::new)
            .transpose()?;

        // Count mode short-circuits pagination and enrichment — report the
        // number of resolved paths, not a page.
        if input.count {
            let paths = self.count_paths(&input, exclude.as_ref())?;
            return Ok(GlobOutcome::Count { paths });
        }

        // cwd-scoped search: present when the original pattern was relative.
        let cwd = input.cwd.as_deref();

        // Run pipeline — handlers return full unpaginated output plus the set
        // of files the output depends on. Existing paths dispatch directly;
        // unexpanded glob patterns are expanded daemon-side.
        let (full_output, mut touched) = self
            .handle_literal_paths(&input.paths, &input, exclude.as_ref(), cwd, parent_id)
            .await?;

        // Paginate first (borrows), then move output into cache. `touched` is the
        // witness set (rendered files + their directories); dedup so repeated
        // dirs aren't re-statted. Their mtimes invalidate the cache on a host
        // edit or a sibling add/remove (bug #26 residual).
        touched.sort();
        touched.dedup();
        let paginated = paginate(&full_output, self.budget, page);
        let roots = self.client_manager.roots();
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(key, full_output, &roots, &touched, &self.fs_manager);
        }

        Ok(GlobOutcome::Rendered(paginated))
    }

    /// Single file: header with defensive map (if symbols available).
    ///
    /// Single files bypass `outline_threshold` — they get a map unless the
    /// grammar is not installed or the path matches `outline_suppress`.
    /// Returns the full unpaginated output.
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
                let _ = writeln!(
                    full,
                    "cwd: {compressed} (no LSP \u{2014} see `catenary roots -h`)"
                );
            }
            path.strip_prefix(cwd).map_or_else(
                |_| path.to_string_lossy().to_string(),
                |rel| rel.to_string_lossy().to_string(),
            )
        } else {
            // Absolute pattern outside workspace roots: LSP warning.
            if self.fs_manager.resolve_root(path).is_none() {
                let _ = writeln!(full, "(no LSP \u{2014} see `catenary roots -h`)");
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

        for sym in syms {
            render_symbol_line(&mut full, sym, Some(&children_set), "\t");
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
    ) -> Result<(String, Vec<PathBuf>)> {
        let resolved = expand_search_paths(paths, input.include_gitignored, input.include_hidden);
        let mut full = String::new();
        // Result-cache witnesses: files (content) and the directories they're
        // listed in (membership) — so a host edit *or* an add/remove of a
        // sibling invalidates a cached page (bug #26). A directory's mtime moves
        // only on a direct entry add/remove/rename, not on a content edit, so
        // witnessing it doesn't cause spurious misses.
        let mut touched: Vec<PathBuf> = Vec::new();
        for path in &resolved {
            if path.is_file() || path.is_symlink() {
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
                touched.push(path.clone());
                // Parent dir: catches a new sibling from a pattern-glob expansion.
                if let Some(parent) = path.parent() {
                    touched.push(parent.to_path_buf());
                }
            } else if path.is_dir() {
                let (output, files) = self
                    .handle_glob_dir(path, input, exclude, cwd, parent_id)
                    .await?;
                full.push_str(&output);
                touched.extend(files);
                // The listed dir itself: catches add/remove/rename of an
                // immediate entry (including subdirs) in the rendered listing.
                touched.push(path.clone());
            }
            // Skip non-existent paths silently — shell expansion
            // shouldn't produce them, but be defensive.
        }
        Ok((full, touched))
    }

    /// Directory listing: enriched (maps) where LSP available, plain (flags) otherwise.
    ///
    /// Collects immediate children, applies visibility and exclude filters,
    /// detects flags (gitignored, snapshot, broken). Output shape is
    /// capability-driven, not volume-driven. Returns the full unpaginated output.
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
    ) -> Result<(String, Vec<PathBuf>)> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

        let entries = self.collect_dir_entries(&canonical, input, exclude)?;

        if entries.is_empty() {
            return Ok(("Directory is empty".to_string(), Vec::new()));
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
                let _ = writeln!(
                    full,
                    "cwd: {compressed} (no LSP \u{2014} see `catenary roots -h`)"
                );
            }
            let display = canonical.strip_prefix(cwd).map_or_else(
                |_| canonical.to_string_lossy().to_string(),
                |rel| rel.to_string_lossy().to_string(),
            );
            let _ = writeln!(full, "{display}/");
        } else {
            // Absolute pattern outside workspace roots: LSP warning.
            if self.fs_manager.resolve_root(&canonical).is_none() {
                let _ = writeln!(full, "(no LSP \u{2014} see `catenary roots -h`)");
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
        // `file_paths` (the dir's eligible files) is the cache's mtime-snapshot
        // set — editing any listed file invalidates the cached listing.
        Ok((full, file_paths))
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
    /// Mirrors [`Self::handle_literal_paths`] dispatch: each resolved file or
    /// symlink counts once; each directory contributes its listed entry count
    /// (the same filtered set [`Self::handle_glob_dir`] renders). LSP
    /// enrichment is skipped — a count is pure filesystem.
    fn count_paths(&self, input: &GlobInput, exclude: Option<&ResolvedGlob>) -> Result<usize> {
        let resolved =
            expand_search_paths(&input.paths, input.include_gitignored, input.include_hidden);
        let mut total = 0usize;
        for path in &resolved {
            if path.is_file() || path.is_symlink() {
                total += 1;
            } else if path.is_dir() {
                let canonical = path
                    .canonicalize()
                    .map_err(|e| anyhow!("Path does not exist: {}: {e}", path.display()))?;
                total += self.collect_dir_entries(&canonical, input, exclude)?.len();
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

/// Renders a single symbol line: `:start-end <Kind[, deprecated]> Name[/]`.
fn render_symbol_line(
    out: &mut String,
    sym: &Symbol,
    children_set: Option<&HashSet<String>>,
    indent: &str,
) {
    let kind_label = format_symbol_kind(&sym.kind);
    let trailing = if children_set.is_some_and(|cs| cs.contains(&sym.name)) {
        "/"
    } else {
        ""
    };
    let deprecated = if sym.deprecated { ", deprecated" } else { "" };
    let _ = writeln!(
        out,
        "{indent}:{}-{} <{kind_label}{deprecated}> {}{trailing}",
        sym.line + 1,
        sym.end_line + 1,
        sym.name,
    );
}

// ─── Structure deduplication ──────────────────────────────────────────

/// Minimum data needed for structure deduplication. Both `GlobEntry`
/// (directory listings) and `FileNode` (glob pattern trees) provide
/// these fields.
struct MapItem<'a> {
    name: &'a str,
    abs_path: &'a Path,
    line_count: Option<usize>,
}

/// Fingerprint: sorted `(kind, name)` pairs as a single string key.
fn make_fingerprint(syms: &[Symbol]) -> String {
    let mut pairs: Vec<(&str, &str)> = syms
        .iter()
        .map(|s| (s.kind.as_str(), s.name.as_str()))
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, n)| format!("{k}\x00{n}"))
        .collect::<Vec<_>>()
        .join("\x01")
}

/// A bounding symbol for a dedup group.
struct BoundingSymbol {
    name: String,
    kind: String,
    min_line: u32,
    max_end_line: u32,
    has_children: bool,
}

/// A shared dedup group: (item indices into the `MapItem` slice, bounding symbols).
type SharedGroup = (Vec<usize>, Vec<BoundingSymbol>);

/// Computes structure dedup groups for a set of map-eligible items.
///
/// Returns `(shared_groups, individual_indices)` where shared groups
/// are items with identical fingerprints (≥2 files) and individuals
/// are unique. Indices are into the `items` slice.
fn compute_dedup(
    items: &[MapItem<'_>],
    outline: &HashMap<PathBuf, Vec<Symbol>>,
    symbol_index: &SymbolIndex,
) -> (Vec<SharedGroup>, Vec<usize>) {
    // Group by fingerprint.
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if let Some(syms) = outline.get(item.abs_path) {
            let fp = make_fingerprint(syms);
            groups.entry(fp).or_default().push(i);
        }
    }

    let mut shared = Vec::new();
    let mut individual = Vec::new();

    for (_fp, indices) in groups {
        if indices.len() == 1 {
            individual.push(indices[0]);
        } else {
            // Compute bounding ranges using the first file as representative.
            let rep_path = items[indices[0]].abs_path;
            let rep_syms = outline.get(rep_path);
            let bounding = rep_syms.map_or_else(Vec::new, |syms| {
                syms.iter()
                    .map(|sym| {
                        let mut min_l = sym.line;
                        let mut max_e = sym.end_line;
                        for &other_idx in &indices[1..] {
                            if let Some(other_syms) = outline.get(items[other_idx].abs_path) {
                                for s in other_syms {
                                    if s.kind == sym.kind && s.name == sym.name {
                                        min_l = min_l.min(s.line);
                                        max_e = max_e.max(s.end_line);
                                    }
                                }
                            }
                        }
                        BoundingSymbol {
                            name: sym.name.clone(),
                            kind: sym.kind.clone(),
                            min_line: min_l,
                            max_end_line: max_e,
                            has_children: symbol_index.has_children(rep_path, &sym.name),
                        }
                    })
                    .collect()
            });
            shared.push((indices, bounding));
        }
    }

    (shared, individual)
}

/// Renders a shared dedup group: file list + "common structure" header
/// + bounding symbols.
fn render_shared_group(
    out: &mut String,
    items: &[MapItem<'_>],
    group_indices: &[usize],
    bounding: &[BoundingSymbol],
    indent: &str,
    sym_indent: &str,
) {
    for &gi in group_indices {
        let item = &items[gi];
        if let Some(lc) = item.line_count {
            let _ = writeln!(out, "{indent}{}  ({lc} lines)", item.name);
        } else {
            let _ = writeln!(out, "{indent}{}", item.name);
        }
    }
    let _ = writeln!(out, "{indent}common structure (ranges are bounding):");
    for sym in bounding {
        let trailing = if sym.has_children { "/" } else { "" };
        let kind_label = format_symbol_kind(&sym.kind);
        let _ = writeln!(
            out,
            "{sym_indent}:{}-{} <{kind_label}> {}{trailing}",
            sym.min_line + 1,
            sym.max_end_line + 1,
            sym.name,
        );
    }
}

/// Renders an individual file's outline symbols.
fn render_individual_map(
    out: &mut String,
    syms: &[Symbol],
    children_set: Option<&HashSet<String>>,
    sym_indent: &str,
) {
    for sym in syms {
        render_symbol_line(out, sym, children_set, sym_indent);
    }
}

// ─── Directory rendering ─────────────────────────────────────────────

/// Renders a directory listing: enriched (maps) for files with LSP
/// symbols, plain (flags) for the rest.
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

    // Build children sets and render enriched for eligible, plain for the rest.
    let children_sets = build_children_sets(idx, &eligible_refs);

    let map_items: Vec<MapItem<'_>> = eligible_indices
        .iter()
        .map(|&i| MapItem {
            name: &entries[i].name,
            abs_path: &entries[i].abs_path,
            line_count: entries[i].line_count,
        })
        .collect();

    let (shared_groups, individual_map_indices) = compute_dedup(&map_items, &outline, idx);

    let mut entry_to_group: HashMap<usize, usize> = HashMap::new();
    for (gi, (mi_indices, _)) in shared_groups.iter().enumerate() {
        for &mi in mi_indices {
            entry_to_group.insert(eligible_indices[mi], gi);
        }
    }
    let individual_entries: HashSet<usize> = individual_map_indices
        .iter()
        .map(|&mi| eligible_indices[mi])
        .collect();

    let mut rendered_groups: HashSet<usize> = HashSet::new();
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
        if let Some(&gi) = entry_to_group.get(&ei) {
            if rendered_groups.contains(&gi) {
                continue;
            }
            rendered_groups.insert(gi);
            let (mi_indices, bounding) = &shared_groups[gi];
            render_shared_group(
                &mut result,
                &map_items,
                mi_indices,
                bounding,
                indent,
                &sym_indent,
            );
        } else if individual_entries.contains(&ei) {
            let flags = compute_entry_flags(f, Some(idx), 0, outline_suppress, fs_manager, true);
            render_entry_line(&mut result, f, &flags, indent);
            if let Some(syms) = outline.get(&f.abs_path) {
                let cs = children_sets.get(&f.abs_path);
                render_individual_map(&mut result, syms, cs, &sym_indent);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use globset::Glob;

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_default_page_deserialization() {
        let input: GlobInput =
            serde_json::from_value(serde_json::json!({"paths": ["src/"]})).expect("deserialize");
        assert_eq!(input.page, 1, "default page should be 1");
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

    // ─── render_symbol_line ─────────────────────────────────────────

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
        render_symbol_line(&mut out, &sym, None, "\t");

        assert_eq!(out, "\t:10-20 <Function> my_func\n");
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
        render_symbol_line(&mut out, &sym, Some(&children), "\t");

        assert_eq!(out, "\t:1-11 <Struct> MyStruct/\n");
    }

    #[test]
    fn test_render_symbol_line_deprecated() {
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
        render_symbol_line(&mut out, &sym, None, "");

        assert_eq!(out, ":5-7 <Function, deprecated> old_fn\n");
    }

    #[test]
    fn test_render_symbol_line_not_in_children_set() {
        let sym = Symbol {
            name: "standalone".to_string(),
            kind: "function".to_string(),
            line: 0,
            end_line: 5,
            scope: None,
            scope_kind: None,
            deprecated: false,
        };

        // Children set exists but doesn't contain this symbol.
        let children: HashSet<String> = ["OtherThing".to_string()].into();
        let mut out = String::new();
        render_symbol_line(&mut out, &sym, Some(&children), "");

        // No trailing slash since not in children set.
        assert_eq!(out, ":1-6 <Function> standalone\n");
    }

    // ─── make_fingerprint ───────────────────────────────────────────

    #[test]
    fn test_make_fingerprint_produces_sorted_pairs() {
        let syms = vec![
            Symbol {
                name: "beta".to_string(),
                kind: "struct".to_string(),
                line: 5,
                end_line: 10,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
            Symbol {
                name: "alpha".to_string(),
                kind: "function".to_string(),
                line: 0,
                end_line: 3,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
        ];

        let fp = make_fingerprint(&syms);

        // Sorted by (kind, name): ("function", "alpha") before ("struct", "beta").
        assert_eq!(fp, "function\x00alpha\x01struct\x00beta");
    }

    #[test]
    fn test_make_fingerprint_identical_for_same_symbols() {
        let syms_a = vec![
            Symbol {
                name: "foo".to_string(),
                kind: "function".to_string(),
                line: 0,
                end_line: 5,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
            Symbol {
                name: "Bar".to_string(),
                kind: "struct".to_string(),
                line: 10,
                end_line: 20,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
        ];

        // Same kinds/names but different line numbers.
        let syms_b = vec![
            Symbol {
                name: "foo".to_string(),
                kind: "function".to_string(),
                line: 3,
                end_line: 8,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
            Symbol {
                name: "Bar".to_string(),
                kind: "struct".to_string(),
                line: 15,
                end_line: 25,
                scope: None,
                scope_kind: None,
                deprecated: false,
            },
        ];

        assert_eq!(
            make_fingerprint(&syms_a),
            make_fingerprint(&syms_b),
            "same symbols at different lines should have identical fingerprints"
        );
    }

    #[test]
    fn test_make_fingerprint_differs_for_different_symbols() {
        let syms_a = vec![Symbol {
            name: "foo".to_string(),
            kind: "function".to_string(),
            line: 0,
            end_line: 5,
            scope: None,
            scope_kind: None,
            deprecated: false,
        }];

        let syms_b = vec![Symbol {
            name: "bar".to_string(),
            kind: "function".to_string(),
            line: 0,
            end_line: 5,
            scope: None,
            scope_kind: None,
            deprecated: false,
        }];

        assert_ne!(
            make_fingerprint(&syms_a),
            make_fingerprint(&syms_b),
            "different symbols should have different fingerprints"
        );
    }

    // ─── compute_dedup ─────────────────────────────────────────────

    fn make_symbol(name: &str, kind: &str, line: u32, end_line: u32) -> Symbol {
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

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_dedup_shared_and_individual() {
        let idx = SymbolIndex::new().expect("create index");

        let path_a = PathBuf::from("/test/a.rs");
        let path_b = PathBuf::from("/test/b.rs");
        let path_c = PathBuf::from("/test/c.rs");

        // a.rs and b.rs have identical symbols (same kind+name → same fingerprint).
        let sym_foo_a = make_symbol("foo", "function", 0, 5);
        let sym_foo_b = make_symbol("foo", "function", 3, 8);
        // c.rs has a different symbol.
        let sym_bar = make_symbol("bar", "struct", 0, 10);

        let mut outline: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        outline.insert(path_a.clone(), vec![sym_foo_a]);
        outline.insert(path_b.clone(), vec![sym_foo_b]);
        outline.insert(path_c.clone(), vec![sym_bar]);

        let items = vec![
            MapItem {
                name: "a.rs",
                abs_path: &path_a,
                line_count: Some(10),
            },
            MapItem {
                name: "b.rs",
                abs_path: &path_b,
                line_count: Some(15),
            },
            MapItem {
                name: "c.rs",
                abs_path: &path_c,
                line_count: Some(20),
            },
        ];

        let (shared, individual) = compute_dedup(&items, &outline, &idx);

        // a.rs and b.rs share the same fingerprint → one shared group.
        assert_eq!(shared.len(), 1, "should have 1 shared group");
        let (group_indices, bounding) = &shared[0];
        assert_eq!(
            group_indices.len(),
            2,
            "shared group should contain 2 items"
        );
        // Both indices should refer to items 0 and 1 (a.rs and b.rs).
        assert!(
            group_indices.contains(&0) && group_indices.contains(&1),
            "shared group should contain a.rs (0) and b.rs (1): {group_indices:?}"
        );

        // Bounding should have one symbol with min/max across both files.
        assert_eq!(bounding.len(), 1, "should have 1 bounding symbol");
        assert_eq!(bounding[0].name, "foo");
        assert_eq!(bounding[0].kind, "function");
        assert_eq!(bounding[0].min_line, 0, "min_line should be min(0, 3)");
        assert_eq!(
            bounding[0].max_end_line, 8,
            "max_end_line should be max(5, 8)"
        );

        // c.rs has a unique fingerprint → individual.
        assert_eq!(individual.len(), 1, "should have 1 individual");
        assert_eq!(individual[0], 2, "individual should be c.rs (index 2)");
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_dedup_all_unique() {
        let idx = SymbolIndex::new().expect("create index");

        let path_a = PathBuf::from("/test/a.rs");
        let path_b = PathBuf::from("/test/b.rs");

        let mut outline: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        outline.insert(path_a.clone(), vec![make_symbol("foo", "function", 0, 5)]);
        outline.insert(path_b.clone(), vec![make_symbol("bar", "function", 0, 5)]);

        let items = vec![
            MapItem {
                name: "a.rs",
                abs_path: &path_a,
                line_count: Some(10),
            },
            MapItem {
                name: "b.rs",
                abs_path: &path_b,
                line_count: Some(10),
            },
        ];

        let (shared, individual) = compute_dedup(&items, &outline, &idx);

        assert!(shared.is_empty(), "no shared groups when all unique");
        assert_eq!(individual.len(), 2, "both should be individual");
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_dedup_bounding_uses_kind_and_name() {
        // When computing bounding ranges, only symbols matching BOTH kind and
        // name should contribute. If the `&&` were replaced with `||`, unrelated
        // symbols would contaminate the bounding range.
        let idx = SymbolIndex::new().expect("create index");

        let path_a = PathBuf::from("/test/a.rs");
        let path_b = PathBuf::from("/test/b.rs");

        // Both files have function "foo" and struct "baz" (same fingerprint).
        // In file B, "baz" has a wider range.
        let mut outline: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        outline.insert(
            path_a.clone(),
            vec![
                make_symbol("foo", "function", 0, 5),
                make_symbol("baz", "struct", 10, 20),
            ],
        );
        outline.insert(
            path_b.clone(),
            vec![
                make_symbol("foo", "function", 2, 4),
                make_symbol("baz", "struct", 8, 25),
            ],
        );

        let items = vec![
            MapItem {
                name: "a.rs",
                abs_path: &path_a,
                line_count: Some(30),
            },
            MapItem {
                name: "b.rs",
                abs_path: &path_b,
                line_count: Some(30),
            },
        ];

        let (shared, _) = compute_dedup(&items, &outline, &idx);

        assert_eq!(shared.len(), 1);
        let (_, bounding) = &shared[0];
        assert_eq!(bounding.len(), 2, "should have 2 bounding symbols");

        // Find "foo" bounding — should use min(0,2)=0, max(5,4)=5.
        let foo = bounding.iter().find(|b| b.name == "foo").expect("foo");
        assert_eq!(foo.min_line, 0, "foo min_line = min(0, 2)");
        assert_eq!(foo.max_end_line, 5, "foo max_end_line = max(5, 4)");

        // Find "baz" bounding — should use min(10,8)=8, max(20,25)=25.
        let baz = bounding.iter().find(|b| b.name == "baz").expect("baz");
        assert_eq!(baz.min_line, 8, "baz min_line = min(10, 8)");
        assert_eq!(baz.max_end_line, 25, "baz max_end_line = max(20, 25)");
    }

    // ─── render_shared_group ───────────────────────────────────────

    #[test]
    fn test_render_shared_group_output_format() {
        let items = vec![
            MapItem {
                name: "alpha.rs",
                abs_path: Path::new("/test/alpha.rs"),
                line_count: Some(100),
            },
            MapItem {
                name: "beta.rs",
                abs_path: Path::new("/test/beta.rs"),
                line_count: None,
            },
        ];

        let bounding = vec![BoundingSymbol {
            name: "foo".to_string(),
            kind: "function".to_string(),
            min_line: 2,
            max_end_line: 9,
            has_children: false,
        }];

        let mut out = String::new();
        render_shared_group(&mut out, &items, &[0, 1], &bounding, "", "\t");

        // File with line count shows "(N lines)".
        assert!(
            out.contains("alpha.rs  (100 lines)"),
            "should show line count for alpha: {out:?}"
        );
        // File without line count shows just the name.
        assert!(
            out.contains("beta.rs\n"),
            "should show name only for beta: {out:?}"
        );
        // Header line.
        assert!(
            out.contains("common structure (ranges are bounding):"),
            "should have common structure header: {out:?}"
        );
        // Symbol line: 0-based to 1-based conversion.
        assert!(
            out.contains("\t:3-10 <Function> foo\n"),
            "should show bounding symbol with 1-based lines: {out:?}"
        );
    }

    #[test]
    fn test_render_shared_group_with_children() {
        let items = vec![MapItem {
            name: "a.rs",
            abs_path: Path::new("/test/a.rs"),
            line_count: Some(50),
        }];

        let bounding = vec![BoundingSymbol {
            name: "MyStruct".to_string(),
            kind: "struct".to_string(),
            min_line: 0,
            max_end_line: 20,
            has_children: true,
        }];

        let mut out = String::new();
        render_shared_group(&mut out, &items, &[0], &bounding, "", "\t");

        assert!(
            out.contains("<Struct> MyStruct/\n"),
            "children should produce trailing slash: {out:?}"
        );
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
}
