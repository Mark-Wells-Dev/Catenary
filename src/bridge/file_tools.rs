// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Glob tool handler: unified file/directory/pattern browsing.
//!
//! The `glob` tool auto-detects intent from the pattern:
//! - File path → single file with defensive map (if grammar installed)
//! - Directory path → listing with line counts, maps, and flags
//! - Glob pattern → recursive file tree with symbols
//!
//! Output shape is determined by LSP coverage, not result volume:
//! - Enriched: file listing with defensive maps from symbol index (LSP available)
//! - Plain: file listing with entry flags (no LSP)
//!
//! When results exceed the budget, output is paged via the `page` parameter.

use anyhow::{Result, anyhow};
use globset::Glob;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::filesystem_manager::{FilesystemManager, format_file_size};
use super::handler::{expand_tilde, resolve_path};
use super::session::ResolvedGlob;
use super::tool_server::ToolServer;
use crate::config::DispatchMethod;
use crate::lsp::LspClientManager;
use crate::lsp::server::LspServer;
use crate::symbol_index::{Symbol, SymbolIndex, format_symbol_kind};

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// File path, directory path, or glob pattern.
    pub pattern: String,
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

// ─── Tree types ──────────────────────────────────────────────────────

/// A directory node in the tree structure for glob pattern results.
struct DirNode {
    dirs: BTreeMap<String, Self>,
    files: Vec<FileNode>,
}

/// A file leaf in the tree structure.
struct FileNode {
    name: String,
    abs_path: PathBuf,
    line_count: Option<usize>,
    binary_size: Option<String>,
    is_gitignored: bool,
    is_snapshot: bool,
}

impl DirNode {
    const fn new() -> Self {
        Self {
            dirs: BTreeMap::new(),
            files: Vec::new(),
        }
    }

    /// Inserts a file at the given path components.
    fn insert(&mut self, components: &[&str], file: FileNode) {
        if components.len() <= 1 {
            self.files.push(file);
        } else {
            let dir = self
                .dirs
                .entry(components[0].to_owned())
                .or_insert_with(Self::new);
            dir.insert(&components[1..], file);
        }
    }

    /// Removes `FileNode` leaves whose name (minus trailing `/`) duplicates
    /// a `DirNode` key at the same level. Recurses into children.
    ///
    /// This happens when `**/*` matches both a directory and files inside
    /// it — the directory appears as a `DirNode` branch (from deeper
    /// matches) and as a `FileNode` leaf (from the directory itself).
    fn prune_dir_dupes(&mut self) {
        self.files
            .retain(|f| !self.dirs.contains_key(f.name.trim_end_matches('/')));
        for child in self.dirs.values_mut() {
            child.prune_dir_dupes();
        }
    }

    /// Renders the tree with tab indentation (plain: flags, no maps).
    fn render_plain(
        &self,
        out: &mut String,
        depth: usize,
        symbol_index: Option<&SymbolIndex>,
        outline_threshold: usize,
        outline_suppress: &[globset::GlobMatcher],
        fs_manager: &FilesystemManager,
    ) {
        let indent: String = "\t".repeat(depth);

        for (name, child) in &self.dirs {
            let _ = writeln!(out, "{indent}{name}/");
            child.render_plain(
                out,
                depth + 1,
                symbol_index,
                outline_threshold,
                outline_suppress,
                fs_manager,
            );
        }

        let mut sorted: Vec<&FileNode> = self.files.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for file in sorted {
            let flags = compute_plain_flags(
                file,
                symbol_index,
                outline_threshold,
                outline_suppress,
                fs_manager,
                false,
            );
            render_file_node(out, file, &indent, &flags);
        }
    }

    /// Renders the tree with tab indentation (enriched: maps + flags + dedup).
    fn render_enriched(
        &self,
        out: &mut String,
        depth: usize,
        outline: &HashMap<PathBuf, Vec<Symbol>>,
        children_sets: &HashMap<PathBuf, HashSet<String>>,
        sa_paths: &HashSet<PathBuf>,
        symbol_index: &SymbolIndex,
    ) {
        let indent: String = "\t".repeat(depth);
        let sym_indent = format!("{indent}\t");

        for (name, child) in &self.dirs {
            let _ = writeln!(out, "{indent}{name}/");
            child.render_enriched(
                out,
                depth + 1,
                outline,
                children_sets,
                sa_paths,
                symbol_index,
            );
        }

        let mut sorted: Vec<(usize, &FileNode)> = self.files.iter().enumerate().collect();
        sorted.sort_by(|a, b| a.1.name.cmp(&b.1.name));

        // Build MapItems for files that have outline data (map-eligible).
        let eligible: Vec<(usize, MapItem<'_>)> = sorted
            .iter()
            .filter(|(_, f)| outline.contains_key(&f.abs_path))
            .map(|&(i, f)| {
                (
                    i,
                    MapItem {
                        name: &f.name,
                        abs_path: &f.abs_path,
                        line_count: f.line_count,
                    },
                )
            })
            .collect();

        let map_items: Vec<MapItem<'_>> = eligible
            .iter()
            .map(|(_, mi)| MapItem {
                name: mi.name,
                abs_path: mi.abs_path,
                line_count: mi.line_count,
            })
            .collect();

        let (shared_groups, individual_map_indices) =
            compute_dedup(&map_items, outline, symbol_index);

        // Build lookup: original file index → shared group index.
        let mut file_to_group: HashMap<usize, usize> = HashMap::new();
        for (gi, (mi_indices, _)) in shared_groups.iter().enumerate() {
            for &mi in mi_indices {
                file_to_group.insert(eligible[mi].0, gi);
            }
        }
        let individual_files: HashSet<usize> = individual_map_indices
            .iter()
            .map(|&mi| eligible[mi].0)
            .collect();

        let mut rendered_groups: HashSet<usize> = HashSet::new();

        for &(fi, file) in &sorted {
            if let Some(&gi) = file_to_group.get(&fi) {
                if rendered_groups.contains(&gi) {
                    continue;
                }
                rendered_groups.insert(gi);

                let (mi_indices, bounding) = &shared_groups[gi];
                render_shared_group(out, &map_items, mi_indices, bounding, &indent, &sym_indent);
            } else if individual_files.contains(&fi) {
                let mut flags = Vec::new();
                if file.is_gitignored {
                    flags.push("gitignored");
                }
                if file.is_snapshot {
                    flags.push("snapshot");
                }
                render_file_node(out, file, &indent, &flags);
                if let Some(syms) = outline.get(&file.abs_path) {
                    let cs = children_sets.get(&file.abs_path);
                    render_individual_map(out, syms, cs, &sym_indent);
                }
            } else {
                // Non-eligible file: flags only.
                // Files reaching this branch are NOT in `outline` (those
                // go through eligible → file_to_group/individual_files),
                // so we only need the sa_paths check.
                let mut flags = Vec::new();
                if sa_paths.contains(&file.abs_path) {
                    flags.push("symbols available");
                }
                if file.is_gitignored {
                    flags.push("gitignored");
                }
                if file.is_snapshot {
                    flags.push("snapshot");
                }
                render_file_node(out, file, &indent, &flags);
            }
        }
    }
}

/// Renders a single `FileNode` line with optional flags.
fn render_file_node(out: &mut String, file: &FileNode, indent: &str, flags: &[&str]) {
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };

    if file.is_snapshot {
        let _ = writeln!(out, "{indent}{} [snapshot]", file.name);
    } else if let Some(ref size) = file.binary_size {
        let _ = writeln!(out, "{indent}{}  ({size}){flag_str}", file.name);
    } else if let Some(lc) = file.line_count {
        let _ = writeln!(out, "{indent}{}  ({lc} lines){flag_str}", file.name);
    } else {
        let _ = writeln!(out, "{indent}{}{flag_str}", file.name);
    }
}

/// Computes flags for a `FileNode` in plain tree rendering.
fn compute_plain_flags<'a>(
    file: &FileNode,
    symbol_index: Option<&SymbolIndex>,
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
    map_rendered: bool,
) -> Vec<&'a str> {
    let mut flags = Vec::new();

    if !map_rendered
        && !file.is_snapshot
        && has_symbols_available(&file.abs_path, symbol_index)
        && (file.line_count.is_some_and(|lc| lc >= outline_threshold)
            || is_outline_suppressed(&file.abs_path, outline_suppress, fs_manager))
    {
        flags.push("symbols available");
    }

    if file.is_gitignored {
        flags.push("gitignored");
    }
    if file.is_snapshot {
        flags.push("snapshot");
    }

    flags
}

// ─── Glob tool server ─────────────────────────────────────────────────

/// Glob tool server: unified file/directory/pattern browsing.
pub struct GlobServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) symbol_index: Option<Arc<Mutex<SymbolIndex>>>,
    pub(super) budget: usize,
    pub(super) outline_threshold: usize,
    pub(super) outline_suppress: Vec<globset::GlobMatcher>,
}

impl ToolServer for GlobServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        _parent_id: Option<i64>,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value> {
        let mut input: GlobInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let pattern = expand_tilde(&input.pattern);
        let path = resolve_path(&pattern)?;

        // Explicit hidden targets (e.g. `.gitignore`, `.github/*`) should
        // match without requiring `include_hidden`. Only applies to glob
        // patterns — resolved file/directory paths go through different
        // branches that don't need the override.
        if !path.exists() && ResolvedGlob::targets_hidden(&pattern) {
            input.include_hidden = true;
        }

        tracing::debug!("glob: {pattern}");

        // Compile exclude pattern if provided. Patterns without a path
        // separator match the basename (like `**/pat`) so the agent can
        // write `exclude="test_*"` instead of `exclude="**/test_*"`.
        let exclude = input
            .exclude
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|pat| {
                let effective = if pat.contains('/') {
                    pat.to_string()
                } else {
                    format!("**/{pat}")
                };
                Glob::new(&effective)
                    .map(|g| g.compile_matcher())
                    .map_err(|e| anyhow!("Invalid exclude pattern: {e}"))
            })
            .transpose()?;

        let page = input.page.max(1);

        // Relative patterns get a `cwd = …` context header.
        let cwd = if PathBuf::from(&pattern).is_absolute() {
            None
        } else {
            Some(std::env::current_dir()?)
        };

        // Run pipeline.
        let output = if path.is_file() || path.is_symlink() {
            self.client_manager
                .ensure_and_wait_for_paths(std::slice::from_ref(&path))
                .await;
            self.ensure_symbols(std::slice::from_ref(&path)).await;
            self.handle_glob_file(&path, page, cwd.as_deref())
        } else if path.is_dir() {
            self.handle_glob_dir(&path, &input, exclude.as_ref(), page, cwd.as_deref())
                .await?
        } else {
            self.handle_glob_pattern(&pattern, &input, exclude.as_ref(), page, cwd.as_deref())
                .await?
        };

        Ok(Value::String(output))
    }
}

impl GlobServer {
    /// Ensures the symbol index is populated for the given files.
    ///
    /// For each file without cached symbols, opens the document on the
    /// server, requests `documentSymbol`, and feeds the response to the
    /// index.
    ///
    /// Not deduplicated against concurrent callers — if parallel MCP tool
    /// calls request the same file, both will do the LSP round-trip. The
    /// index write is serialized by `Mutex` so the result is correct, just
    /// redundant. To optimize: add a `pending: Mutex<HashSet<PathBuf>>` to
    /// `SymbolIndex` and skip files already in-flight.
    async fn ensure_symbols(&self, files: &[PathBuf]) {
        let Some(ref idx_arc) = self.symbol_index else {
            return;
        };
        let needs_populate: Vec<PathBuf> = {
            let Ok(idx) = idx_arc.lock() else { return };
            files
                .iter()
                .filter(|p| p.is_file() && !idx.has_symbols_for(p))
                .cloned()
                .collect()
        };

        for path in &needs_populate {
            let servers = self
                .client_manager
                .get_servers(
                    path,
                    LspServer::supports_document_symbols,
                    Some(DispatchMethod::DocumentSymbol),
                )
                .await;
            let Some(server) = servers.first() else {
                continue;
            };
            let Ok(uri) = self
                .client_manager
                .open_document_on(path, server, None)
                .await
            else {
                continue;
            };
            let Ok(response) = server.lock().await.document_symbols(&uri).await else {
                continue;
            };
            if let Ok(idx) = idx_arc.lock() {
                let _ = idx.populate_from_document_symbols(path, &response);
            }
        }
    }

    /// Single file: header with defensive map (if symbols available).
    ///
    /// Single files bypass `outline_threshold` — they get a map unless the
    /// grammar is not installed or the path matches `outline_suppress`. Pages
    /// when the map exceeds budget.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    #[allow(
        clippy::option_if_let_else,
        reason = "side-effecting writeln in the Some branch"
    )]
    fn handle_glob_file(&self, path: &Path, page: usize, cwd: Option<&Path>) -> String {
        let mut full = String::new();

        // Context header: `cwd = …` for relative patterns, absolute path for absolute.
        let display = if let Some(cwd) = cwd {
            let _ = writeln!(full, "cwd = {}", cwd.display());
            path.strip_prefix(cwd).map_or_else(
                |_| path.to_string_lossy().to_string(),
                |rel| rel.to_string_lossy().to_string(),
            )
        } else {
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
            return format_page_header(1, 1) + &full;
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
            return format_page_header(1, 1) + &full;
        };
        let Ok(idx) = ts_arc.lock() else {
            return format_page_header(1, 1) + &full;
        };
        if !idx.has_symbols_for(path)
            || is_outline_suppressed(path, &self.outline_suppress, &self.fs_manager)
        {
            return format_page_header(1, 1) + &full;
        }

        let Ok(outline) = idx.query_outline_batch(&[path]) else {
            return format_page_header(1, 1) + &full;
        };
        let Some(syms) = outline.get(path) else {
            return format_page_header(1, 1) + &full;
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

        paginate(&full, self.budget, page)
    }

    /// Directory listing: enriched (maps) where LSP available, plain (flags) otherwise.
    ///
    /// Collects immediate children, applies visibility and exclude filters,
    /// detects flags (gitignored, snapshot, broken). Output shape is
    /// capability-driven, not volume-driven. Paged via `page` parameter.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    async fn handle_glob_dir(
        &self,
        dir: &Path,
        input: &GlobInput,
        exclude: Option<&globset::GlobMatcher>,
        page: usize,
        cwd: Option<&Path>,
    ) -> Result<String> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

        // Build non-gitignored set for flag detection.
        let non_ignored: HashSet<PathBuf> = if input.include_gitignored {
            WalkBuilder::new(&canonical)
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

        let walker = WalkBuilder::new(&canonical)
            .max_depth(Some(1))
            .git_ignore(!input.include_gitignored)
            .hidden(!input.include_hidden)
            .build();

        let mut entries = Vec::new();

        for entry in walker.flatten() {
            let entry_path = entry.into_path();
            if entry_path == canonical {
                continue;
            }

            let name = entry_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            // Apply exclude filter against the entry name.
            if let Some(matcher) = exclude
                && matcher.is_match(&name)
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
        self.ensure_symbols(&file_paths).await;

        let ts_guard = self.symbol_index.as_ref().and_then(|m| m.lock().ok());

        // Context header: `cwd = …` for relative, absolute path for absolute.
        let mut full = String::new();
        if let Some(cwd) = cwd {
            let _ = writeln!(full, "cwd = {}", cwd.display());
            let display = canonical.strip_prefix(cwd).map_or_else(
                |_| canonical.to_string_lossy().to_string(),
                |rel| rel.to_string_lossy().to_string(),
            );
            let _ = writeln!(full, "{display}/");
        } else {
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
        Ok(paginate(&full, self.budget, page))
    }

    /// Glob pattern match across workspace roots with tree output.
    ///
    /// Output shape is capability-driven: enriched (maps) for files with LSP
    /// symbols, plain (flags) for files without. Paged via `page` parameter.
    ///
    /// Absolute patterns (e.g. `/home/user/projects/*`) are searched from
    /// the pattern's base directory rather than workspace roots.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    #[allow(
        clippy::option_if_let_else,
        reason = "if-let reads better for large divergent branches"
    )]
    async fn handle_glob_pattern(
        &self,
        pattern: &str,
        input: &GlobInput,
        exclude: Option<&globset::GlobMatcher>,
        page: usize,
        cwd: Option<&Path>,
    ) -> Result<String> {
        let resolved = ResolvedGlob::new(pattern)?;

        let search_roots = if let Some(override_root) = resolved.override_root() {
            vec![override_root.to_path_buf()]
        } else {
            let roots = self.client_manager.roots();
            if roots.is_empty() {
                vec![std::env::current_dir()?]
            } else {
                roots
            }
        };

        // Build non-gitignored set for flag detection.
        let non_ignored: HashSet<PathBuf> = if input.include_gitignored {
            let mut set = HashSet::new();
            for root in &search_roots {
                let walker = WalkBuilder::new(root)
                    .git_ignore(true)
                    .hidden(!input.include_hidden)
                    .build();
                set.extend(walker.flatten().map(ignore::DirEntry::into_path));
            }
            set
        } else {
            HashSet::new()
        };

        // (abs, root, gitignored, is_dir)
        let mut matched_entries: Vec<(PathBuf, PathBuf, bool, bool)> = Vec::new();

        for root in &search_roots {
            let walker = WalkBuilder::new(root)
                .git_ignore(!input.include_gitignored)
                .hidden(!input.include_hidden)
                .build();

            for entry in walker.flatten() {
                let ft = entry.file_type();
                let is_file = ft.is_some_and(|ft| ft.is_file());
                let is_dir = ft.is_some_and(|ft| ft.is_dir());
                if !is_file && !is_dir {
                    continue;
                }

                let entry_path = entry.path();

                // Skip the search root itself (walker emits it as first entry).
                if is_dir && entry_path == root.as_path() {
                    continue;
                }

                if resolved.is_match(entry_path, root) {
                    if let Some(matcher) = exclude
                        && matcher.is_match(entry_path.strip_prefix(root).unwrap_or(entry_path))
                    {
                        continue;
                    }
                    let gitignored = input.include_gitignored && !non_ignored.contains(entry_path);
                    matched_entries.push((
                        entry_path.to_path_buf(),
                        root.clone(),
                        gitignored,
                        is_dir,
                    ));
                }
            }
        }

        matched_entries.sort_by(|a, b| a.0.cmp(&b.0));
        matched_entries.dedup_by(|a, b| a.0 == b.0);

        if matched_entries.is_empty() {
            return Ok("No matches found".to_string());
        }

        // Build tree. Relative patterns: paths relative to cwd.
        // Absolute patterns: paths relative to the (single) override root.
        let tree_root = if let Some(cwd) = cwd {
            cwd.to_path_buf()
        } else {
            search_roots[0].clone()
        };

        let mut node = DirNode::new();
        let mut files: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
        for (abs_path, _, gitignored, is_dir) in &matched_entries {
            let rel = abs_path.strip_prefix(&tree_root).unwrap_or(abs_path);
            let rel_str = rel.to_string_lossy();
            let components: Vec<&str> = rel_str.split('/').collect();
            Self::insert_entry(
                &mut node,
                &mut files,
                abs_path,
                &tree_root,
                *gitignored,
                *is_dir,
                &components,
                self,
            );
        }
        node.prune_dir_dupes();

        // Populate symbol index for matched files (not directories).
        let file_paths: Vec<PathBuf> = matched_entries
            .iter()
            .filter(|(_, _, _, is_dir)| !is_dir)
            .map(|(p, _, _, _)| p.clone())
            .collect();
        self.client_manager
            .ensure_and_wait_for_paths(&file_paths)
            .await;
        self.ensure_symbols(&file_paths).await;

        // Render: enriched (maps) where LSP available, plain (flags) otherwise.
        let ts_guard = self.symbol_index.as_ref().and_then(|m| m.lock().ok());
        let ts_ref = ts_guard.as_deref();

        let mut full = String::new();

        // Context header for relative patterns.
        if let Some(cwd) = cwd {
            let _ = writeln!(full, "cwd = {}", cwd.display());
        }

        // For absolute patterns, the root path is the section header.
        if cwd.is_none() {
            let _ = writeln!(full, "{}", tree_root.display());
        }

        let base_depth = usize::from(cwd.is_none());
        let mut section_content = String::new();
        if let Some(idx) = ts_ref {
            let group_abs: Vec<PathBuf> = files.iter().map(|(p, _, _)| p.clone()).collect();
            let eligible: Vec<&Path> = group_abs
                .iter()
                .filter(|p| {
                    is_enrichment_eligible(
                        p,
                        &files,
                        self.outline_threshold,
                        &self.outline_suppress,
                        idx,
                        &self.fs_manager,
                    )
                })
                .map(PathBuf::as_path)
                .collect();

            if !eligible.is_empty()
                && let Ok(outline) = idx.query_outline_batch(&eligible)
                && !outline.is_empty()
            {
                let children_sets = build_children_sets(idx, &eligible);
                let sa_paths = build_sa_paths(&files, idx);
                node.render_enriched(
                    &mut section_content,
                    base_depth,
                    &outline,
                    &children_sets,
                    &sa_paths,
                    idx,
                );
            }
        }

        // Fall back to plain if enriched produced nothing.
        if section_content.is_empty() {
            node.render_plain(
                &mut section_content,
                base_depth,
                ts_ref,
                self.outline_threshold,
                &self.outline_suppress,
                &self.fs_manager,
            );
        }

        full.push_str(&section_content);

        Ok(paginate(&full, self.budget, page))
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

    /// Inserts a matched entry into a tree node and files list.
    #[allow(
        clippy::too_many_arguments,
        reason = "avoids struct wrapper for one call site"
    )]
    fn insert_entry(
        node: &mut DirNode,
        files: &mut Vec<(PathBuf, PathBuf, bool)>,
        abs_path: &Path,
        root: &Path,
        gitignored: bool,
        is_dir: bool,
        components: &[&str],
        server: &Self,
    ) {
        if is_dir {
            let dir_name = components.last().unwrap_or(&"").to_string();
            let display_name = format!("{dir_name}/");
            files.push((abs_path.to_path_buf(), root.to_path_buf(), gitignored));
            node.insert(
                components,
                FileNode {
                    name: display_name,
                    abs_path: abs_path.to_path_buf(),
                    line_count: None,
                    binary_size: None,
                    is_gitignored: gitignored,
                    is_snapshot: false,
                },
            );
        } else {
            let metadata = std::fs::metadata(abs_path).ok();
            let file_name = components.last().unwrap_or(&"").to_string();
            let snap = is_snapshot(&file_name);

            let (line_count, binary_size) = if snap {
                (None, None)
            } else {
                server.file_info(abs_path, metadata.as_ref())
            };

            files.push((abs_path.to_path_buf(), root.to_path_buf(), gitignored));
            node.insert(
                components,
                FileNode {
                    name: file_name,
                    abs_path: abs_path.to_path_buf(),
                    line_count,
                    binary_size,
                    is_gitignored: gitignored,
                    is_snapshot: snap,
                },
            );
        }
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

/// Returns `true` if a matched file in a glob pattern tree is map-eligible.
fn is_enrichment_eligible(
    path: &Path,
    _matched_files: &[(PathBuf, PathBuf, bool)],
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    symbol_index: &SymbolIndex,
    fs_manager: &FilesystemManager,
) -> bool {
    let metadata = std::fs::metadata(path).ok();
    let line_count = metadata
        .as_ref()
        .and_then(|m| fs_manager.line_count(path, m));

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    !is_snapshot(&name)
        && line_count.is_some_and(|lc| lc >= outline_threshold)
        && symbol_index.has_symbols_for(path)
        && !is_outline_suppressed(path, outline_suppress, fs_manager)
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

// ─── Page-based paging ───────────────────────────────────────────────

/// Formats the page header: `[page N/M]\n\n`.
fn format_page_header(page: usize, total: usize) -> String {
    format!("[page {page}/{total}]\n\n")
}

/// Splits full output into pages and returns the requested page with a header.
///
/// Pages are split at line boundaries so no line is broken mid-way.
/// The budget is the maximum character count per page (excluding the header).
fn paginate(full: &str, budget: usize, page: usize) -> String {
    let lines: Vec<&str> = full.lines().collect();
    if lines.is_empty() {
        return format_page_header(1, 1);
    }

    // Build pages by accumulating lines until budget is hit.
    let mut pages: Vec<(usize, usize)> = Vec::new(); // (start_line, end_line) exclusive
    let mut start = 0;
    let mut current_len = 0;

    for (i, line) in lines.iter().enumerate() {
        let line_len = line.len() + 1; // +1 for newline
        if current_len > 0 && current_len + line_len > budget {
            pages.push((start, i));
            start = i;
            current_len = 0;
        }
        current_len += line_len;
    }
    // Final page.
    if start < lines.len() {
        pages.push((start, lines.len()));
    }

    let total = pages.len();
    let idx = (page.max(1) - 1).min(total.saturating_sub(1));

    if idx >= pages.len() {
        return format!("[page {page}/{total}]\n\nNo more results.");
    }

    let (s, e) = pages[idx];
    let mut out = format_page_header(idx + 1, total);
    for &line in &lines[s..e] {
        out.push_str(line);
        out.push('\n');
    }
    out
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

/// Builds the set of paths that have grammars available (for `[symbols available]`).
fn build_sa_paths(
    matched_files: &[(PathBuf, PathBuf, bool)],
    symbol_index: &SymbolIndex,
) -> HashSet<PathBuf> {
    matched_files
        .iter()
        .filter(|(p, _, _)| symbol_index.has_symbols_for(p))
        .map(|(p, _, _)| p.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_per_root_tier2_rendering() {
        let mut node = DirNode::new();

        node.insert(
            &["main.rs"],
            FileNode {
                name: "main.rs".to_string(),
                abs_path: PathBuf::from("/test/root/main.rs"),
                line_count: Some(10),
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );
        node.insert(
            &["lib.rs"],
            FileNode {
                name: "lib.rs".to_string(),
                abs_path: PathBuf::from("/test/root/lib.rs"),
                line_count: Some(5),
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );

        let mut tier2 = String::new();
        node.render_plain(&mut tier2, 0, None, 200, &[], &FilesystemManager::new());

        // Files sorted alphabetically with exact format.
        assert_eq!(
            tier2, "lib.rs  (5 lines)\nmain.rs  (10 lines)\n",
            "render_plain should produce sorted file listing with line counts"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_render_plain_nested_indentation() {
        let mut node = DirNode::new();

        node.insert(
            &["sub", "inner.rs"],
            FileNode {
                name: "inner.rs".to_string(),
                abs_path: PathBuf::from("/test/sub/inner.rs"),
                line_count: Some(3),
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );
        node.insert(
            &["top.rs"],
            FileNode {
                name: "top.rs".to_string(),
                abs_path: PathBuf::from("/test/top.rs"),
                line_count: Some(5),
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );

        let mut out = String::new();
        node.render_plain(&mut out, 0, None, 200, &[], &FilesystemManager::new());

        // Dirs at depth 0 (no tab), nested files at depth 1 (one tab).
        assert_eq!(
            out, "sub/\n\tinner.rs  (3 lines)\ntop.rs  (5 lines)\n",
            "nested directory should increase indentation depth"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_plain_flags_symbols_available() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/big.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "foo",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let file = FileNode {
            name: "big.rs".to_string(),
            abs_path: path,
            line_count: Some(200),
            binary_size: None,
            is_gitignored: false,
            is_snapshot: false,
        };
        let fs = FilesystemManager::new();

        // Above threshold, symbols exist, not rendered, not snapshot.
        let flags = compute_plain_flags(&file, Some(&idx), 100, &[], &fs, false);
        assert_eq!(flags, vec!["symbols available"]);

        // map_rendered = true suppresses the flag.
        let flags = compute_plain_flags(&file, Some(&idx), 100, &[], &fs, true);
        assert!(
            !flags.contains(&"symbols available"),
            "map_rendered should suppress flag: {flags:?}"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_plain_flags_snapshot_suppresses() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/snap.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "bar",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let file = FileNode {
            name: "snap.rs".to_string(),
            abs_path: path,
            line_count: Some(200),
            binary_size: None,
            is_gitignored: false,
            is_snapshot: true,
        };
        let fs = FilesystemManager::new();

        let flags = compute_plain_flags(&file, Some(&idx), 100, &[], &fs, false);
        assert!(
            !flags.contains(&"symbols available"),
            "snapshot should suppress symbols available: {flags:?}"
        );
        assert_eq!(flags, vec!["snapshot"]);
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_plain_flags_below_threshold() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/small.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "tiny",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 7 } }
            }]),
        )
        .expect("populate");

        let file = FileNode {
            name: "small.rs".to_string(),
            abs_path: path,
            line_count: Some(50),
            binary_size: None,
            is_gitignored: false,
            is_snapshot: false,
        };
        let fs = FilesystemManager::new();

        // Below threshold, no suppress → no flag.
        let flags = compute_plain_flags(&file, Some(&idx), 100, &[], &fs, false);
        assert!(
            flags.is_empty(),
            "below threshold should have no flags: {flags:?}"
        );
    }

    #[test]
    fn test_compute_plain_flags_gitignored() {
        let file = FileNode {
            name: "debug.log".to_string(),
            abs_path: PathBuf::from("/test/debug.log"),
            line_count: Some(10),
            binary_size: None,
            is_gitignored: true,
            is_snapshot: false,
        };
        let fs = FilesystemManager::new();

        let flags = compute_plain_flags(&file, None, 200, &[], &fs, false);
        assert_eq!(flags, vec!["gitignored"]);
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_compute_plain_flags_suppressed_below_threshold() {
        let idx = SymbolIndex::new().expect("create index");
        let path = PathBuf::from("/test/suppressed.rs");

        idx.populate_from_document_symbols(
            &path,
            &serde_json::json!([{
                "name": "func",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 7 } }
            }]),
        )
        .expect("populate");

        let file = FileNode {
            name: "suppressed.rs".to_string(),
            abs_path: path,
            line_count: Some(50), // below threshold
            binary_size: None,
            is_gitignored: false,
            is_snapshot: false,
        };
        let fs = FilesystemManager::new();

        // Below threshold BUT outline_suppress matches → flag via || branch.
        let suppress = vec![
            Glob::new("**/*.rs")
                .expect("compile glob")
                .compile_matcher(),
        ];
        let flags = compute_plain_flags(&file, Some(&idx), 100, &suppress, &fs, false);
        assert_eq!(
            flags,
            vec!["symbols available"],
            "suppressed file below threshold should still have flag"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_render_enriched_symbols_available_flag() {
        let idx = SymbolIndex::new().expect("create index");

        // File A: has symbols in outline → gets individual map.
        let path_a = PathBuf::from("/test/a.rs");
        idx.populate_from_document_symbols(
            &path_a,
            &serde_json::json!([{
                "name": "foo",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate a");

        // File B: has symbols in index but NOT in outline.
        let path_b = PathBuf::from("/test/b.rs");
        idx.populate_from_document_symbols(
            &path_b,
            &serde_json::json!([{
                "name": "bar",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate b");

        let mut node = DirNode::new();
        node.insert(
            &["a.rs"],
            FileNode {
                name: "a.rs".to_string(),
                abs_path: path_a.clone(),
                line_count: Some(10),
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );
        node.insert(
            &["b.rs"],
            FileNode {
                name: "b.rs".to_string(),
                abs_path: path_b.clone(),
                line_count: Some(5),
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );

        // Outline only includes a.rs — b.rs is non-eligible.
        let mut outline = HashMap::new();
        outline.insert(
            path_a.clone(),
            vec![Symbol {
                name: "foo".to_string(),
                kind: "function".to_string(),
                line: 0,
                end_line: 2,
                scope: None,
                scope_kind: None,
                deprecated: false,
            }],
        );

        let children_sets = HashMap::new();
        let sa_paths: HashSet<PathBuf> = [path_a, path_b].into();

        let mut out = String::new();
        node.render_enriched(&mut out, 0, &outline, &children_sets, &sa_paths, &idx);

        // a.rs should have its symbol map rendered.
        assert!(
            out.contains("<Function> foo"),
            "a.rs should have symbol in map: {out:?}"
        );
        // b.rs should show [symbols available] (in sa_paths, not in outline).
        let b_line = out
            .lines()
            .find(|l| l.contains("b.rs"))
            .expect("b.rs in output");
        assert!(
            b_line.contains("[symbols available]"),
            "b.rs should have [symbols available]: {b_line}"
        );
        // a.rs header should NOT have [symbols available] (it has a map).
        let a_line = out
            .lines()
            .find(|l| l.contains("a.rs"))
            .expect("a.rs in output");
        assert!(
            !a_line.contains("[symbols available]"),
            "a.rs with map should not have [symbols available]: {a_line}"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_default_page_deserialization() {
        let input: GlobInput =
            serde_json::from_value(serde_json::json!({"pattern": "*.rs"})).expect("deserialize");
        assert_eq!(input.page, 1, "default page should be 1");
    }

    #[test]
    fn test_paginate_single_page() {
        let content = "line 1\nline 2\nline 3\n";
        let result = paginate(content, 5000, 1);
        assert!(
            result.starts_with("[page 1/1]"),
            "single-page result should show [page 1/1]: {result}"
        );
        assert!(
            result.contains("line 1"),
            "should contain content: {result}"
        );
    }

    #[test]
    fn test_paginate_multi_page() {
        // Each line is ~7 chars. Budget of 20 should give ~3 lines per page.
        let content = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";
        let result = paginate(content, 20, 1);
        assert!(
            result.contains("[page 1/"),
            "should have page header: {result}"
        );
        // Page 2 should have different content.
        let page2 = paginate(content, 20, 2);
        assert!(
            page2.contains("[page 2/"),
            "page 2 should have page header: {page2}"
        );
    }

    #[test]
    fn test_paginate_beyond_last() {
        let content = "line 1\nline 2\n";
        let result = paginate(content, 5000, 99);
        // Should clamp to last page.
        assert!(
            result.contains("line 1"),
            "beyond-last should show last page: {result}"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_prune_dir_dupes() {
        let mut node = DirNode::new();

        // Simulate `**/*` matching both "sub" (dir) and "sub/file.rs" (file).
        // The dir match inserts a FileNode leaf at root level.
        node.insert(
            &["sub"],
            FileNode {
                name: "sub/".to_string(),
                abs_path: PathBuf::from("/r/sub"),
                line_count: None,
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );
        // The file match creates a DirNode branch and inserts inside it.
        node.insert(
            &["sub", "file.rs"],
            FileNode {
                name: "file.rs".to_string(),
                abs_path: PathBuf::from("/r/sub/file.rs"),
                line_count: Some(10),
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            },
        );

        // Before prune: root has both a "sub" DirNode and a "sub/" FileNode.
        assert_eq!(node.files.len(), 1, "should have dir leaf before prune");
        assert_eq!(node.dirs.len(), 1, "should have dir branch");

        node.prune_dir_dupes();

        // After prune: the duplicate FileNode leaf is removed.
        assert!(
            node.files.is_empty(),
            "dir leaf should be pruned: {:?}",
            node.files.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert_eq!(node.dirs.len(), 1, "dir branch should remain");

        // The file inside the DirNode should be unaffected.
        let sub = node.dirs.get("sub").expect("sub dir should exist");
        assert_eq!(sub.files.len(), 1, "nested file should remain");
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

    // ─── is_enrichment_eligible (pattern tree) ────────────────────────

    /// Generates a string with `n` lines for tests that need files of a
    /// specific line count.
    fn gen_lines(n: usize) -> String {
        "x\n".repeat(n)
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_regular_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("big.rs");
        // 200 lines to exceed threshold of 100.
        std::fs::write(&path, gen_lines(200)).expect("write file");

        let idx = SymbolIndex::new().expect("create index");
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

        let fs = FilesystemManager::new();
        let matched = vec![(path.clone(), dir.path().to_path_buf(), false)];

        assert!(
            is_enrichment_eligible(&path, &matched, 100, &[], &idx, &fs),
            "regular file above threshold with symbols should be eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_snapshot_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("handler.catenary_snapshot_5.rs");
        std::fs::write(&path, gen_lines(200)).expect("write file");

        let idx = SymbolIndex::new().expect("create index");
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

        let fs = FilesystemManager::new();
        let matched = vec![(path.clone(), dir.path().to_path_buf(), false)];

        assert!(
            !is_enrichment_eligible(&path, &matched, 100, &[], &idx, &fs),
            "snapshot file should not be enrichment eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_below_threshold() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("small.rs");
        // 10 lines, below threshold of 100.
        std::fs::write(&path, gen_lines(10)).expect("write file");

        let idx = SymbolIndex::new().expect("create index");
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

        let fs = FilesystemManager::new();
        let matched = vec![(path.clone(), dir.path().to_path_buf(), false)];

        assert!(
            !is_enrichment_eligible(&path, &matched, 100, &[], &idx, &fs),
            "file below threshold should not be eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_no_symbols() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("no_syms.rs");
        std::fs::write(&path, gen_lines(200)).expect("write file");

        let idx = SymbolIndex::new().expect("create index");
        // Don't populate symbols.
        let fs = FilesystemManager::new();
        let matched = vec![(path.clone(), dir.path().to_path_buf(), false)];

        assert!(
            !is_enrichment_eligible(&path, &matched, 100, &[], &idx, &fs),
            "file without symbols should not be eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_suppressed() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("denied.rs");
        std::fs::write(&path, gen_lines(200)).expect("write file");

        let idx = SymbolIndex::new().expect("create index");
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

        let suppress = vec![
            Glob::new("**/*.rs")
                .expect("compile glob")
                .compile_matcher(),
        ];
        let fs = FilesystemManager::new();
        let matched = vec![(path.clone(), dir.path().to_path_buf(), false)];

        assert!(
            !is_enrichment_eligible(&path, &matched, 100, &suppress, &idx, &fs),
            "suppressed file should not be eligible"
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_enrichment_eligible_at_threshold_boundary() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("boundary.rs");
        // Exactly 100 lines for threshold of 100.
        std::fs::write(&path, gen_lines(100)).expect("write file");

        let idx = SymbolIndex::new().expect("create index");
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

        let fs = FilesystemManager::new();
        let matched = vec![(path.clone(), dir.path().to_path_buf(), false)];

        assert!(
            is_enrichment_eligible(&path, &matched, 100, &[], &idx, &fs),
            "file at exact threshold should be eligible"
        );

        // One line below threshold.
        let path_below = dir.path().join("below.rs");
        std::fs::write(&path_below, gen_lines(99)).expect("write file");

        idx.populate_from_document_symbols(
            &path_below,
            &serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }]),
        )
        .expect("populate");

        let matched_below = vec![(path_below.clone(), dir.path().to_path_buf(), false)];
        assert!(
            !is_enrichment_eligible(&path_below, &matched_below, 100, &[], &idx, &fs),
            "file one below threshold should not be eligible"
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
}
