// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Glob tool handler: unified file/directory/pattern browsing.
//!
//! The `glob` tool auto-detects intent from the pattern:
//! - File path → single file with defensive map (if grammar installed)
//! - Directory path → listing with line counts, maps, and flags
//! - Glob pattern → recursive file tree, tiered output
//!
//! Three tiers with promote-from-bottom selection:
//! - Tier 3: bucketed glob patterns with counts (always fits)
//! - Tier 2: file listing with entry flags (`[symbols available]`, etc.)
//! - Tier 1: file listing with defensive maps from symbol index

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
use crate::bucketing::{self, BucketEntry};
use crate::config::DispatchMethod;
use crate::lsp::LspClientManager;
use crate::lsp::server::LspServer;
use crate::symbol_index::{ScopeFilter, Symbol, SymbolIndex, format_symbol_kind};

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// File path, directory path, or glob pattern.
    pub pattern: String,
    /// Symbol path to drill into (wired in 08c).
    #[serde(default)]
    pub into: Option<String>,
    /// Glob pattern to exclude from results.
    #[serde(default)]
    pub exclude: Option<String>,
    /// Continuation token from previous result.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include gitignored files (default: false).
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files (default: false).
    #[serde(default)]
    pub include_hidden: bool,
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

// ─── Into types ──────────────────────────────────────────────────────

/// A parsed segment from an `into` path.
struct IntoSegment {
    /// Kind filter (e.g., `"function"`, `"*"` for any, or `None`).
    kind: Option<String>,
    /// Tag filters (e.g., `["deprecated"]`).
    tags: Vec<String>,
    /// Glob pattern on the symbol name.
    name_pattern: String,
    /// True for `**` recursive match segments.
    is_recursive: bool,
}

/// Parses an `into` path into segments.
///
/// Split on `/`. Each segment:
/// 1. If starts with `<`: extract comma-separated labels up to `>`.
///    First label is kind (or `*` for any). Subsequent are tags.
///    Remainder after `> ` is the name pattern.
/// 2. If the segment is `**`: recursive match marker.
/// 3. Otherwise: entire segment is the name pattern.
fn parse_into(into: &str) -> Vec<IntoSegment> {
    let raw: Vec<&str> = into.split('/').filter(|s| !s.is_empty()).collect();
    let mut segments = Vec::new();
    let mut i = 0;

    while i < raw.len() {
        let seg = raw[i];

        if seg == "**" {
            // `**` merges with the following segment as AnyDepth.
            // `**/X` → AnyDepth query for X.
            // `**` alone → AnyDepth query for `*`.
            if i + 1 < raw.len() {
                let next = raw[i + 1];
                let mut parsed = parse_single_segment(next);
                parsed.is_recursive = true;
                segments.push(parsed);
                i += 2;
            } else {
                segments.push(IntoSegment {
                    kind: None,
                    tags: Vec::new(),
                    name_pattern: "*".to_string(),
                    is_recursive: true,
                });
                i += 1;
            }
            continue;
        }

        segments.push(parse_single_segment(seg));
        i += 1;
    }

    segments
}

/// Parses a single `into` segment (without `**` handling).
fn parse_single_segment(seg: &str) -> IntoSegment {
    if let Some(rest) = seg.strip_prefix('<')
        && let Some((qualifiers, name)) = rest.split_once("> ")
    {
        let mut parts = qualifiers.split(',').map(str::trim);
        let kind_raw = parts.next().unwrap_or("*");
        let kind = if kind_raw == "*" {
            None
        } else {
            Some(kind_raw.to_lowercase())
        };
        let tags: Vec<String> = parts.map(str::to_lowercase).collect();
        return IntoSegment {
            kind,
            tags,
            name_pattern: name.to_string(),
            is_recursive: false,
        };
    }

    IntoSegment {
        kind: None,
        tags: Vec::new(),
        name_pattern: seg.to_string(),
        is_recursive: false,
    }
}

/// Expands `{a,b}` alternation in a glob pattern into separate patterns.
///
/// `SQLite` GLOB doesn't support `{a,b}` syntax. This function expands
/// it into separate patterns that are queried individually and merged.
/// Returns a single-element vec if no alternation is present.
fn expand_alternation(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(close) = pattern[open..].find('}') else {
        return vec![pattern.to_string()];
    };
    let close = open + close;

    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let alternatives = pattern[open + 1..close].split(',');

    alternatives
        .map(|alt| format!("{prefix}{alt}{suffix}"))
        .collect()
}

/// Converts a kind filter from display format back to the capture suffix
/// stored in the index. `"Impl"` → `"implementation"`, others lowercase.
fn kind_to_capture(kind: &str) -> String {
    if kind.eq_ignore_ascii_case("impl") {
        "implementation".to_string()
    } else {
        kind.to_lowercase()
    }
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

    /// Inserts a bare path (no metadata) for `into` display trees.
    fn insert_path(&mut self, components: &[&str], abs_path: PathBuf) {
        if components.len() <= 1 {
            let name = components.first().unwrap_or(&"").to_string();
            self.files.push(FileNode {
                name,
                abs_path,
                line_count: None,
                binary_size: None,
                is_gitignored: false,
                is_snapshot: false,
            });
        } else {
            let dir = self
                .dirs
                .entry(components[0].to_owned())
                .or_insert_with(Self::new);
            dir.insert_path(&components[1..], abs_path);
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

    /// Returns true if this tree has any subdirectories.
    fn has_dirs(&self) -> bool {
        !self.dirs.is_empty()
    }

    /// Renders the tree with tab indentation (tier 2: flags, no maps).
    fn render_tier2(
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
            child.render_tier2(
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
            let flags = compute_tree_flags(
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

    /// Renders the tree with tab indentation (tier 1: maps + flags + dedup).
    fn render_tier1(
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
            child.render_tier1(
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
                let has_map = outline.contains_key(&file.abs_path);
                let mut flags = Vec::new();
                if sa_paths.contains(&file.abs_path) && !has_map {
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

    /// Renders the tree for `into` output with symbol chains at each file.
    fn render_into(
        &self,
        out: &mut String,
        depth: usize,
        chains: &HashMap<PathBuf, Vec<Vec<Symbol>>>,
        symbol_index: &SymbolIndex,
        expand_children: bool,
    ) {
        let indent: String = "\t".repeat(depth);

        for (name, child) in &self.dirs {
            let _ = writeln!(out, "{indent}{name}/");
            child.render_into(out, depth + 1, chains, symbol_index, expand_children);
        }

        let mut sorted: Vec<&FileNode> = self.files.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for file in sorted {
            let Some(file_chains) = chains.get(&file.abs_path) else {
                continue;
            };
            if file_chains.is_empty() {
                continue;
            }

            let _ = writeln!(out, "{indent}{}", file.name);

            let children_set = build_children_set_for_file(symbol_index, &file.abs_path);
            render_chains_at_depth(
                out,
                file_chains,
                &children_set,
                expand_children,
                symbol_index,
                &file.abs_path,
                depth + 1,
            );
        }
    }
}

/// Builds a `DirNode` display tree from file paths, stripping their
/// common directory prefix.
///
/// Returns `(common_prefix_header, tree)`. The prefix header is `Some`
/// when the tree has directory structure and the paths share a common
/// ancestor — it is rendered as the root line before the tree.
fn build_display_tree(files: &[&PathBuf]) -> (Option<String>, DirNode) {
    if files.is_empty() {
        return (None, DirNode::new());
    }

    let paths_lossy: Vec<String> = files
        .iter()
        .map(|f| f.to_string_lossy().to_string())
        .collect();
    let all_components: Vec<Vec<&str>> =
        paths_lossy.iter().map(|s| s.split('/').collect()).collect();

    let first = &all_components[0];
    let mut common_len = 0;
    'outer: for i in 0..first.len().saturating_sub(1) {
        for comps in &all_components {
            if i >= comps.len() || comps[i] != first[i] {
                break 'outer;
            }
        }
        common_len = i + 1;
    }

    let mut tree = DirNode::new();
    for (fi, comps) in all_components.iter().enumerate() {
        let rel: Vec<&str> = comps[common_len..].to_vec();
        tree.insert_path(&rel, files[fi].clone());
    }

    let prefix = if common_len > 0 && tree.has_dirs() {
        Some(format!("{}/", first[..common_len].join("/")))
    } else {
        None
    };

    (prefix, tree)
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

/// Computes flags for a `FileNode` in tree rendering (tier 2).
fn compute_tree_flags<'a>(
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

/// Glob tool server: unified file/directory/pattern browsing with tiered output.
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

        // Branch: into pipeline or normal glob pipeline.
        if let Some(ref into_str) = input.into {
            let after_line = input.cursor.as_deref().map(decode_cursor).transpose()?;
            let files = self.resolve_files(&path, &pattern, &input, exclude.as_ref())?;
            self.client_manager.ensure_and_wait_for_paths(&files).await;
            self.ensure_symbols(&files).await;
            let output = self.handle_into(&files, into_str, after_line)?;
            return Ok(Value::String(output));
        }

        // Decode cursor if provided.
        let after_line = input.cursor.as_deref().map(decode_cursor).transpose()?;

        // Run pipeline.
        let output = if path.is_file() || path.is_symlink() {
            self.client_manager
                .ensure_and_wait_for_paths(std::slice::from_ref(&path))
                .await;
            self.ensure_symbols(std::slice::from_ref(&path)).await;
            self.handle_glob_file(&path, after_line)
        } else if path.is_dir() {
            self.handle_glob_dir(&path, &input, exclude.as_ref())
                .await?
        } else {
            self.handle_glob_pattern(&pattern, &input, exclude.as_ref())
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
    fn handle_glob_file(&self, path: &Path, after_line: Option<u32>) -> String {
        let mut result = String::new();

        // Root header.
        let root = self.fs_manager.resolve_root(path);
        let display = root.as_ref().map_or_else(
            || path.to_string_lossy().to_string(),
            |r| {
                path.strip_prefix(r).map_or_else(
                    |_| path.to_string_lossy().to_string(),
                    |rel| rel.to_string_lossy().to_string(),
                )
            },
        );
        match &root {
            Some(r) => {
                let _ = writeln!(result, "Root: {}", r.display());
            }
            None => {
                let _ = writeln!(result, "OutOfRoots:");
            }
        }

        let metadata = std::fs::metadata(path).ok();

        // Detect snapshot or broken symlink.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_snapshot(&name) {
            let _ = writeln!(result, "{display} [snapshot]");
            return result;
        }

        // File header with line count or size.
        let line_count = metadata
            .as_ref()
            .and_then(|m| self.fs_manager.line_count(path, m));
        if let Some(lc) = line_count {
            let _ = writeln!(result, "{display}  ({lc} lines)");
        } else {
            let size = metadata.map_or(0, |m| m.len());
            let _ = writeln!(result, "{display}  ({})", format_file_size(size));
        }

        // Single-file map: bypass threshold, check symbols + deny only.
        let Some(ref ts_arc) = self.symbol_index else {
            return result;
        };
        let Ok(idx) = ts_arc.lock() else {
            return result;
        };
        if !idx.has_symbols_for(path)
            || is_outline_suppressed(path, &self.outline_suppress, &self.fs_manager)
        {
            return result;
        }

        let Ok(outline) = idx.query_outline_batch(&[path]) else {
            return result;
        };
        let Some(syms) = outline.get(path) else {
            return result;
        };

        // Apply cursor: skip symbols at or before `after_line`.
        let filtered: Vec<&Symbol> = syms
            .iter()
            .filter(|s| after_line.is_none_or(|al| s.line > al))
            .collect();

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

        let mut last_line: Option<u32> = None;
        for sym in &filtered {
            let mut line_buf = String::new();
            render_symbol_line(&mut line_buf, sym, Some(&children_set), "\t");
            if result.len() + line_buf.len() > self.budget {
                // Over budget — emit cursor.
                if let Some(ll) = last_line {
                    let _ = writeln!(result, "[cursor: {}]", encode_cursor(ll));
                }
                return result;
            }
            result.push_str(&line_buf);
            last_line = Some(sym.line);
        }

        result
    }

    /// Directory listing with tier selection.
    ///
    /// Collects immediate children, applies visibility and exclude filters,
    /// detects flags (gitignored, snapshot, broken), then selects tier
    /// based on budget.
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
        let tier_output = select_dir_tier(
            &entries,
            self.budget,
            ts_guard.as_deref(),
            self.outline_threshold,
            &self.outline_suppress,
            &self.fs_manager,
        );

        // Root header.
        let root = self.fs_manager.resolve_root(&canonical);
        let mut result = String::new();
        match &root {
            Some(r) => {
                let _ = writeln!(result, "Root: {}", r.display());
            }
            None => {
                let _ = writeln!(result, "OutOfRoots:");
            }
        }
        result.push_str(&tier_output);
        Ok(result)
    }

    /// Glob pattern match across workspace roots with tree output.
    ///
    /// Absolute patterns (e.g. `/home/user/projects/*`) are searched from
    /// the pattern's base directory rather than workspace roots.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    async fn handle_glob_pattern(
        &self,
        pattern: &str,
        input: &GlobInput,
        exclude: Option<&globset::GlobMatcher>,
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

        // Group by search root and build one tree per group.
        let mut grouped: BTreeMap<&PathBuf, Vec<&(PathBuf, PathBuf, bool, bool)>> = BTreeMap::new();
        for entry in &matched_entries {
            grouped.entry(&entry.1).or_default().push(entry);
        }

        let mut sections: Vec<GlobSection> = Vec::new();
        for (root, entries) in &grouped {
            let mut node = DirNode::new();
            let mut flat_names = Vec::new();
            let mut files = Vec::new();

            for (abs_path, _, gitignored, is_dir) in entries.iter().copied() {
                let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
                let rel_str = rel.to_string_lossy();
                let components: Vec<&str> = rel_str.split('/').collect();

                if *is_dir {
                    // Directory entries: trailing `/`, no line count, no enrichment.
                    let dir_name = components.last().unwrap_or(&"").to_string();
                    let display_name = format!("{dir_name}/");
                    flat_names.push(rel_str.to_string());
                    files.push((abs_path.clone(), (*root).clone(), *gitignored));
                    node.insert(
                        &components,
                        FileNode {
                            name: display_name,
                            abs_path: abs_path.clone(),
                            line_count: None,
                            binary_size: None,
                            is_gitignored: *gitignored,
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
                        self.file_info(abs_path, metadata.as_ref())
                    };

                    flat_names.push(rel_str.to_string());
                    files.push((abs_path.clone(), (*root).clone(), *gitignored));

                    node.insert(
                        &components,
                        FileNode {
                            name: file_name,
                            abs_path: abs_path.clone(),
                            line_count,
                            binary_size,
                            is_gitignored: *gitignored,
                            is_snapshot: snap,
                        },
                    );
                }
            }

            // Remove directory FileNode leaves that duplicate DirNode
            // branches (happens with `**/*` matching both a dir and its
            // contents).
            node.prune_dir_dupes();

            sections.push(GlobSection {
                root: (*root).clone(),
                node,
                flat_names,
                files,
            });
        }

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

        // Global tier selection: promote from bottom across all roots.
        let ts_guard = self.symbol_index.as_ref().and_then(|m| m.lock().ok());
        let ts_ref = ts_guard.as_deref();

        // Root header overhead: not counted against the content budget.
        let header_overhead: usize = sections
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let header = format!("Root: {}\n", s.root.display());
                let separator = usize::from(i > 0); // blank line between sections
                header.len() + separator
            })
            .sum();
        let content_budget = self.budget.saturating_sub(header_overhead);

        // Helper: assemble root headers around per-section content.
        let assemble = |per_section: &[String]| -> String {
            let mut out = String::new();
            for (s, content) in sections.iter().zip(per_section) {
                if !out.is_empty() {
                    out.push('\n');
                }
                let _ = writeln!(out, "Root: {}", s.root.display());
                out.push_str(content);
            }
            out
        };

        // Tier 4 (bucketed) — always fits.
        let t4: Vec<String> = sections
            .iter()
            .map(|s| render_bucketed(&s.flat_names, content_budget))
            .collect();

        // Tier 3 (directory + count) — collapse matched files into their
        // immediate parent directory with a match count.
        let t3: Vec<String> = sections
            .iter()
            .map(|s| render_dir_count(&s.flat_names))
            .collect();
        let t3_content: usize = t3.iter().map(String::len).sum();
        if t3_content > content_budget {
            return Ok(assemble(&t4));
        }

        // Tier 2 (tree listing with flags) — promote if content fits.
        let t2: Vec<String> = sections
            .iter()
            .map(|s| {
                let mut out = String::new();
                s.node.render_tier2(
                    &mut out,
                    0,
                    ts_ref,
                    self.outline_threshold,
                    &self.outline_suppress,
                    &self.fs_manager,
                );
                out
            })
            .collect();
        let t2_content: usize = t2.iter().map(String::len).sum();
        if t2_content > content_budget {
            return Ok(assemble(&t3));
        }

        // Tier 1 (tree listing with maps) — promote if has content and fits.
        if let Some(idx) = ts_ref {
            let t1: Vec<String> = sections
                .iter()
                .map(|s| {
                    let group_abs: Vec<PathBuf> =
                        s.files.iter().map(|(p, _, _)| p.clone()).collect();
                    let eligible: Vec<&Path> = group_abs
                        .iter()
                        .filter(|p| {
                            is_map_eligible(
                                p,
                                &s.files,
                                self.outline_threshold,
                                &self.outline_suppress,
                                idx,
                                &self.fs_manager,
                            )
                        })
                        .map(PathBuf::as_path)
                        .collect();

                    if eligible.is_empty() {
                        return String::new();
                    }
                    let Ok(outline) = idx.query_outline_batch(&eligible) else {
                        return String::new();
                    };
                    if outline.is_empty() {
                        return String::new();
                    }

                    let children_sets = build_children_sets(idx, &eligible);
                    let sa_paths = build_sa_paths(&s.files, idx);
                    let mut out = String::new();
                    s.node
                        .render_tier1(&mut out, 0, &outline, &children_sets, &sa_paths, idx);
                    out
                })
                .collect();

            let t1_content: usize = t1.iter().map(String::len).sum();
            if t1_content > 0 && t1_content <= content_budget {
                return Ok(assemble(&t1));
            }
        }

        Ok(assemble(&t2))
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

    /// Resolves the pattern to a list of file paths for the `into` pipeline.
    fn resolve_files(
        &self,
        path: &Path,
        pattern: &str,
        input: &GlobInput,
        exclude: Option<&globset::GlobMatcher>,
    ) -> Result<Vec<PathBuf>> {
        if path.is_file() || path.is_symlink() {
            return Ok(vec![path.to_path_buf()]);
        }

        if path.is_dir() {
            let canonical = path
                .canonicalize()
                .map_err(|e| anyhow!("Path does not exist: {}: {e}", path.display()))?;

            let walker = WalkBuilder::new(&canonical)
                .max_depth(Some(1))
                .git_ignore(!input.include_gitignored)
                .hidden(!input.include_hidden)
                .build();

            let mut files = Vec::new();
            for entry in walker.flatten() {
                let entry_path = entry.into_path();
                if entry_path == canonical {
                    continue;
                }
                let meta = entry_path.symlink_metadata().ok();
                let is_file = meta
                    .as_ref()
                    .is_some_and(|m| m.is_file() || m.file_type().is_symlink());
                if !is_file {
                    continue;
                }
                let name = entry_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Some(matcher) = exclude
                    && matcher.is_match(&name)
                {
                    continue;
                }
                files.push(entry_path);
            }
            return Ok(files);
        }

        // Glob pattern.
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

        let mut matched: Vec<PathBuf> = Vec::new();
        for root in &search_roots {
            let walker = WalkBuilder::new(root)
                .git_ignore(!input.include_gitignored)
                .hidden(!input.include_hidden)
                .build();
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let entry_path = entry.path();
                if resolved.is_match(entry_path, root) {
                    if let Some(matcher) = exclude
                        && matcher.is_match(entry_path.strip_prefix(root).unwrap_or(entry_path))
                    {
                        continue;
                    }
                    matched.push(entry_path.to_path_buf());
                }
            }
        }
        matched.sort();
        matched.dedup();
        Ok(matched)
    }

    /// Handles glob with the `into` parameter: structural symbol navigation.
    ///
    /// Navigates the symbol tree segment by segment, shows target symbols
    /// with their children (or "no nested definitions" for leaves).
    /// `outline_suppress` does NOT apply — `into` is explicit navigation.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    fn handle_into(
        &self,
        files: &[PathBuf],
        into: &str,
        after_line: Option<u32>,
    ) -> Result<String> {
        let Some(ref ts_arc) = self.symbol_index else {
            return Err(anyhow!("`into` requires a symbol index"));
        };
        let idx = ts_arc.lock().map_err(|e| anyhow!("lock error: {e}"))?;

        if files.is_empty() {
            return Ok("No matches found".to_string());
        }

        let segments = parse_into(into);
        if segments.is_empty() {
            return Ok("No matching symbols found".to_string());
        }

        // Walk segments to find target symbols.
        // State: per-file, a list of (scope_name, matched_symbol) pairs
        // representing the navigation context.
        let file_refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();

        // First segment: depth-0 matching.
        let first = &segments[0];
        let scope = if first.is_recursive {
            ScopeFilter::AnyDepth
        } else {
            ScopeFilter::TopLevel
        };
        let kind_filter = first.kind.as_ref().map(|k| kind_to_capture(k));
        let deprecated_only = first.tags.iter().any(|t| t == "deprecated");

        let mut current: HashMap<PathBuf, Vec<Symbol>> = query_with_alternation(
            &idx,
            &file_refs,
            &scope,
            &first.name_pattern,
            kind_filter.as_deref(),
            deprecated_only,
        )?;

        // Track the chain of intermediate symbols for rendering.
        let mut chains: HashMap<PathBuf, Vec<Vec<Symbol>>> = HashMap::new();

        // Initialize chains from first segment matches.
        for (path, syms) in &current {
            let file_chains: Vec<Vec<Symbol>> = syms.iter().map(|s| vec![s.clone()]).collect();
            chains.insert(path.clone(), file_chains);
        }

        // Subsequent segments: children of previous matches.
        for seg in &segments[1..] {
            let mut next: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
            let mut next_chains: HashMap<PathBuf, Vec<Vec<Symbol>>> = HashMap::new();

            let seg_kind = seg.kind.as_ref().map(|k| kind_to_capture(k));
            let seg_deprecated = seg.tags.iter().any(|t| t == "deprecated");

            for (path, prev_syms) in &current {
                let prev_file_chains = chains.get(path).cloned().unwrap_or_default();
                let path_ref: &Path = path;

                for (ci, parent) in prev_syms.iter().enumerate() {
                    let scope_filter = if seg.is_recursive {
                        // `**` after a matched symbol: constrain to the
                        // parent's span so we only find descendants.
                        ScopeFilter::WithinSpan(parent.line, parent.end_line)
                    } else {
                        ScopeFilter::ChildrenOf(&parent.name)
                    };

                    let matches = query_with_alternation(
                        &idx,
                        &[path_ref],
                        &scope_filter,
                        &seg.name_pattern,
                        seg_kind.as_deref(),
                        seg_deprecated,
                    )?;

                    if let Some(syms) = matches.get(path) {
                        let existing = next.entry(path.clone()).or_default();
                        let existing_chains = next_chains.entry(path.clone()).or_default();

                        for sym in syms {
                            existing.push(sym.clone());
                            let mut chain = prev_file_chains.get(ci).cloned().unwrap_or_default();
                            chain.push(sym.clone());
                            existing_chains.push(chain);
                        }
                    }
                }
            }

            current = next;
            chains = next_chains;
        }

        // Filter to files with matches.
        let matched_files: Vec<&PathBuf> = files
            .iter()
            .filter(|f| current.get(*f).is_some_and(|v| !v.is_empty()))
            .collect();

        if matched_files.is_empty() {
            return Ok("No matching symbols found".to_string());
        }

        // Render results with tier selection and paging.
        let result = render_into_results(
            &matched_files,
            &current,
            &chains,
            &idx,
            self.budget,
            after_line,
        );

        Ok(result)
    }
}

// ─── Map eligibility ──────────────────────────────────────────────────

/// Returns `true` if a file is eligible for defensive maps in a directory listing.
fn is_map_eligible_entry(
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
fn is_map_eligible(
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

// ─── Tier selection and rendering ─────────────────────────────────────

/// Selects the best tier for a directory listing and renders it.
///
/// Promote-from-bottom:
/// 1. Render tier 3 (bucketed). Always succeeds.
/// 2. Render tier 2 (file listing with flags). If fits → promote.
/// 3. Render tier 1 (file listing with defensive maps). If fits → promote.
fn select_dir_tier(
    entries: &[GlobEntry],
    budget: usize,
    symbol_index: Option<&SymbolIndex>,
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> String {
    // Collect entry names for bucketing (files and directories).
    let entry_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();

    // 1. Tier 3 (bucketed) — always succeeds.
    let tier3 = render_bucketed(&entry_names, budget);

    // 2. Tier 2 (file listing with flags).
    let tier2 = render_dir_listing_with_flags(
        entries,
        symbol_index,
        outline_threshold,
        outline_suppress,
        fs_manager,
    );
    if tier2.len() > budget {
        return tier3;
    }

    // 3. Tier 1 (file listing with maps).
    let Some(idx) = symbol_index else {
        return tier2;
    };

    let eligible_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            is_map_eligible_entry(e, outline_threshold, outline_suppress, idx, fs_manager)
        })
        .map(|(i, _)| i)
        .collect();

    if eligible_indices.is_empty() {
        return tier2;
    }

    // Ensure fresh for all eligible files.
    // Query outline symbols.
    let eligible_refs: Vec<&Path> = eligible_indices
        .iter()
        .map(|&i| entries[i].abs_path.as_path())
        .collect();
    let Ok(outline) = idx.query_outline_batch(&eligible_refs) else {
        return tier2;
    };

    if outline.is_empty() {
        return tier2;
    }

    // Build children sets and render tier 1.
    let children_sets = build_children_sets(idx, &eligible_refs);
    let tier1 = render_dir_listing_with_maps(
        entries,
        &eligible_indices,
        &outline,
        &children_sets,
        idx,
        symbol_index,
        outline_suppress,
        fs_manager,
    );

    if tier1.len() <= budget { tier1 } else { tier2 }
}

/// Renders a flat directory listing with entry flags (tier 2).
fn render_dir_listing_with_flags(
    entries: &[GlobEntry],
    symbol_index: Option<&SymbolIndex>,
    outline_threshold: usize,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> String {
    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    let mut files: Vec<&GlobEntry> = entries.iter().filter(|e| !e.is_dir).collect();

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let mut result = String::new();

    for d in &dirs {
        let _ = writeln!(result, "{}", d.name);
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
        render_entry_line(&mut result, f, &flags);
    }

    result
}

/// Renders a flat directory listing with defensive maps (tier 1).
#[allow(
    clippy::too_many_arguments,
    reason = "tier 1 rendering needs full context"
)]
fn render_dir_listing_with_maps(
    entries: &[GlobEntry],
    eligible_indices: &[usize],
    outline: &HashMap<PathBuf, Vec<Symbol>>,
    children_sets: &HashMap<PathBuf, HashSet<String>>,
    symbol_index: &SymbolIndex,
    ts_opt: Option<&SymbolIndex>,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> String {
    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));

    let mut files: Vec<(usize, &GlobEntry)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_dir)
        .collect();
    files.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    // Build MapItems from eligible entries.
    let map_items: Vec<MapItem<'_>> = eligible_indices
        .iter()
        .map(|&i| MapItem {
            name: &entries[i].name,
            abs_path: &entries[i].abs_path,
            line_count: entries[i].line_count,
        })
        .collect();

    let (shared_groups, individual_map_indices) = compute_dedup(&map_items, outline, symbol_index);

    // Lookup tables: entry index → shared group, entry index → individual.
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

    for d in &dirs {
        let _ = writeln!(result, "{}", d.name);
    }

    for &(idx, f) in &files {
        if let Some(&gi) = entry_to_group.get(&idx) {
            if rendered_groups.contains(&gi) {
                continue;
            }
            rendered_groups.insert(gi);

            let (mi_indices, bounding) = &shared_groups[gi];
            render_shared_group(&mut result, &map_items, mi_indices, bounding, "", "\t");
        } else if individual_entries.contains(&idx) {
            let flags = compute_entry_flags(f, ts_opt, 0, outline_suppress, fs_manager, true);
            render_entry_line(&mut result, f, &flags);
            if let Some(syms) = outline.get(&f.abs_path) {
                let cs = children_sets.get(&f.abs_path);
                render_individual_map(&mut result, syms, cs, "\t");
            }
        } else {
            // Non-eligible file.
            let has_sa = has_symbols_available(&f.abs_path, ts_opt)
                && is_outline_suppressed(&f.abs_path, outline_suppress, fs_manager);
            let mut flags = Vec::new();
            if has_sa {
                flags.push("symbols available");
            }
            if f.is_gitignored {
                flags.push("gitignored");
            }
            if f.is_snapshot {
                flags.push("snapshot");
            }
            if f.is_broken_symlink {
                flags.push("broken");
            }
            render_entry_line(&mut result, f, &flags);
        }
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
fn render_entry_line(out: &mut String, entry: &GlobEntry, flags: &[&str]) {
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };

    if entry.is_broken_symlink {
        let target = entry.symlink_target.as_deref().unwrap_or("?");
        let _ = writeln!(out, "{} -> {target} [broken]", entry.name);
    } else if entry.is_snapshot {
        let _ = writeln!(out, "{} [snapshot]", entry.name);
    } else if entry.is_symlink {
        let target = entry.symlink_target.as_deref().unwrap_or("?");
        if let Some(lc) = entry.line_count {
            let _ = writeln!(out, "{} -> {target}  ({lc} lines){flag_str}", entry.name);
        } else if let Some(ref size) = entry.binary_size {
            let _ = writeln!(out, "{} -> {target}  ({size}){flag_str}", entry.name);
        } else {
            let _ = writeln!(out, "{} -> {target}{flag_str}", entry.name);
        }
    } else if let Some(ref size) = entry.binary_size {
        let _ = writeln!(out, "{}  ({size}){flag_str}", entry.name);
    } else if let Some(lc) = entry.line_count {
        let _ = writeln!(out, "{}  ({lc} lines){flag_str}", entry.name);
    } else {
        let _ = writeln!(out, "{}{flag_str}", entry.name);
    }
}

// ─── Into rendering ──────────────────────────────────────────────────

/// Renders `into` results with promote-from-bottom tier selection.
///
/// Three tiers:
/// 1. Full — target symbols with children and spans.
/// 2. Degraded — target symbols with spans only, no children.
/// 3. Bucketed — glob patterns grouping files that contain the symbol.
///
/// Structure deduplication: files with identical target children sets
/// are grouped under one representative, same as defensive map dedup.
#[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
fn render_into_results(
    files: &[&PathBuf],
    targets: &HashMap<PathBuf, Vec<Symbol>>,
    chains: &HashMap<PathBuf, Vec<Vec<Symbol>>>,
    symbol_index: &SymbolIndex,
    budget: usize,
    after_line: Option<u32>,
) -> String {
    // Build display tree for proper directory structure.
    let (prefix, tree) = build_display_tree(files);

    // Tier 3 (bucketed): file names with counts.
    let file_names: Vec<String> = files
        .iter()
        .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect();
    let tier3 = render_bucketed(&file_names, budget);

    // Tier 2 (degraded): target symbols with spans, no children.
    let tier2 = render_into_with_prefix(&tree, prefix.as_deref(), chains, symbol_index, false);
    if tier2.len() > budget {
        return tier3;
    }

    // Compute structure dedup for tier 1.
    let dedup = compute_into_dedup(files, targets, chains, symbol_index);

    // Tier 1 (full): target symbols with children + dedup.
    if dedup.shared.is_empty() {
        // No dedup needed — render tree with children.
        let tier1 = render_into_with_prefix(&tree, prefix.as_deref(), chains, symbol_index, true);
        if tier1.len() <= budget {
            return tier1;
        }
        if files.len() == 1 {
            return page_into_output(&tier1, budget, after_line);
        }
    } else {
        // Dedup groups exist — render flat with dedup (tree + dedup
        // interaction is complex; flat rendering is correct).
        let tier1 = render_into_tier1_dedup(files, targets, chains, symbol_index, &dedup);
        if tier1.len() <= budget {
            return tier1;
        }
    }

    tier2
}

/// Pages `into` output for single-file results that exceed budget.
fn page_into_output(full: &str, budget: usize, after_line: Option<u32>) -> String {
    let lines: Vec<&str> = full.lines().collect();

    // Skip lines up to cursor position.
    let start = after_line.map_or(0, |al| {
        lines
            .iter()
            .position(|l| {
                l.trim_start()
                    .strip_prefix(':')
                    .and_then(|r| r.split_once('-'))
                    .and_then(|(s, _)| s.parse::<u32>().ok())
                    .is_some_and(|line_num| line_num > al)
            })
            .unwrap_or(lines.len())
    });

    let mut out = String::new();
    let mut last_line_num: Option<u32> = None;

    for &line in &lines[start..] {
        let candidate = format!("{line}\n");
        if !out.is_empty() && out.len() + candidate.len() > budget {
            if let Some(ll) = last_line_num {
                let _ = writeln!(out, "[cursor: {}]", encode_cursor(ll));
            }
            return out;
        }
        out.push_str(&candidate);

        // Track the 1-based line number from `:N-M` spans.
        if let Some(num) = line
            .trim_start()
            .strip_prefix(':')
            .and_then(|r| r.split_once('-'))
            .and_then(|(s, _)| s.parse::<u32>().ok())
        {
            last_line_num = Some(num);
        }
    }

    out
}

/// Fingerprint for `into` structure dedup: target names + children (kind, name).
fn compute_into_dedup(
    files: &[&PathBuf],
    targets: &HashMap<PathBuf, Vec<Symbol>>,
    chains: &HashMap<PathBuf, Vec<Vec<Symbol>>>,
    symbol_index: &SymbolIndex,
) -> IntoDedup {
    let mut fingerprints: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, &path) in files.iter().enumerate() {
        let Some(file_targets) = targets.get(path) else {
            continue;
        };
        let Some(file_chains) = chains.get(path) else {
            continue;
        };
        if file_chains.is_empty() {
            continue;
        }

        // Build fingerprint from target children.
        let children_set = build_children_set_for_file(symbol_index, path);
        let mut fp_parts: Vec<String> = Vec::new();

        for target in file_targets {
            let children = symbol_index
                .query_scoped(
                    &[path.as_path()],
                    &ScopeFilter::ChildrenOf(&target.name),
                    "*",
                    None,
                    false,
                )
                .ok()
                .and_then(|m| m.get(path).cloned())
                .unwrap_or_default();

            let mut child_pairs: Vec<(&str, &str)> = children
                .iter()
                .map(|c| (c.kind.as_str(), c.name.as_str()))
                .collect();
            child_pairs.sort_unstable();

            let target_fp = format!(
                "{}\x00{}\x01{}",
                target.kind,
                target.name,
                child_pairs
                    .iter()
                    .map(|(k, n)| format!("{k}\x02{n}"))
                    .collect::<Vec<_>>()
                    .join("\x03")
            );
            fp_parts.push(target_fp);
        }

        // Also include chain depth to avoid merging different navigation paths.
        let chain_depth = file_chains.iter().map(Vec::len).max().unwrap_or(0);
        let fp = format!("{chain_depth}\x04{}", fp_parts.join("\x05"));

        fingerprints.entry(fp).or_default().push(i);

        let _ = children_set; // used above in query_scoped
    }

    let shared: Vec<Vec<usize>> = fingerprints
        .into_values()
        .filter(|indices| indices.len() >= 2)
        .collect();

    IntoDedup { shared }
}

/// Structure dedup result for `into`.
struct IntoDedup {
    /// Groups of file indices with identical target children.
    shared: Vec<Vec<usize>>,
}

/// Renders `into` output using the display tree with optional prefix header.
fn render_into_with_prefix(
    tree: &DirNode,
    prefix: Option<&str>,
    chains: &HashMap<PathBuf, Vec<Vec<Symbol>>>,
    symbol_index: &SymbolIndex,
    expand_children: bool,
) -> String {
    let mut out = String::new();

    if let Some(pfx) = prefix {
        let _ = writeln!(out, "{pfx}");
        tree.render_into(&mut out, 1, chains, symbol_index, expand_children);
    } else {
        tree.render_into(&mut out, 0, chains, symbol_index, expand_children);
    }

    out
}

/// Renders chains at a specific indentation depth.
fn render_chains_at_depth(
    out: &mut String,
    file_chains: &[Vec<Symbol>],
    children_set: &HashSet<String>,
    expand_children: bool,
    symbol_index: &SymbolIndex,
    file_path: &Path,
    base_depth: usize,
) {
    let max_chain = file_chains.iter().map(Vec::len).max().unwrap_or(0);
    if max_chain == 0 {
        return;
    }

    if max_chain == 1 {
        let mut seen_lines: HashSet<u32> = HashSet::new();
        let indent: String = "\t".repeat(base_depth);
        let child_indent: String = "\t".repeat(base_depth + 1);
        for chain in file_chains {
            let sym = &chain[0];
            if !seen_lines.insert(sym.line) {
                continue;
            }
            render_into_symbol(out, sym, children_set, &indent);
            if expand_children {
                render_target_children(
                    out,
                    sym,
                    children_set,
                    symbol_index,
                    file_path,
                    &child_indent,
                );
            }
        }
        return;
    }

    render_chain_tree(
        out,
        file_chains,
        0,
        children_set,
        expand_children,
        symbol_index,
        file_path,
        base_depth,
    );
}

/// Renders `into` tier 1 with structure dedup (flat, no tree nesting).
///
/// Used when dedup groups exist — tree structure would fragment the groups.
fn render_into_tier1_dedup(
    files: &[&PathBuf],
    targets: &HashMap<PathBuf, Vec<Symbol>>,
    chains: &HashMap<PathBuf, Vec<Vec<Symbol>>>,
    symbol_index: &SymbolIndex,
    dedup: &IntoDedup,
) -> String {
    let mut out = String::new();
    let mut rendered_in_group: HashSet<usize> = HashSet::new();

    for group in &dedup.shared {
        for &fi in group {
            let name = files[fi]
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let _ = writeln!(out, "{name}");
            rendered_in_group.insert(fi);
        }

        let rep = files[group[0]];
        if let Some(file_chains) = chains.get(rep) {
            let children_set = build_children_set_for_file(symbol_index, rep);
            let _ = writeln!(out, "common structure (ranges are bounding):");
            render_chains_at_depth(
                &mut out,
                file_chains,
                &children_set,
                true,
                symbol_index,
                rep,
                1,
            );
        }
    }

    for (i, &path) in files.iter().enumerate() {
        if rendered_in_group.contains(&i) {
            continue;
        }
        let Some(file_chains) = chains.get(path) else {
            continue;
        };
        if file_chains.is_empty() {
            continue;
        }

        let file_targets = targets.get(path);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let show_line_count =
            file_chains.iter().all(|c| c.len() == 1) && file_targets.is_some_and(|t| t.len() > 2);
        if show_line_count {
            let lc = std::fs::read_to_string(path)
                .ok()
                .map(|c| c.lines().count());
            if let Some(lc) = lc {
                let _ = writeln!(out, "{name}  ({lc} lines)");
            } else {
                let _ = writeln!(out, "{name}");
            }
        } else {
            let _ = writeln!(out, "{name}");
        }

        let children_set = build_children_set_for_file(symbol_index, path);
        render_chains_at_depth(
            &mut out,
            file_chains,
            &children_set,
            true,
            symbol_index,
            path,
            1,
        );
    }

    out
}

/// Recursively renders a chain tree at the given depth level.
#[allow(
    clippy::too_many_arguments,
    reason = "recursive tree rendering needs full context"
)]
fn render_chain_tree(
    out: &mut String,
    chains: &[Vec<Symbol>],
    depth: usize,
    children_set: &HashSet<String>,
    expand_children: bool,
    symbol_index: &SymbolIndex,
    file_path: &Path,
    indent_level: usize,
) {
    // Group chains by the symbol at this depth (keyed by line number).
    let mut groups: BTreeMap<u32, Vec<&Vec<Symbol>>> = BTreeMap::new();
    for chain in chains {
        if depth < chain.len() {
            groups.entry(chain[depth].line).or_default().push(chain);
        }
    }

    let indent: String = "\t".repeat(indent_level);

    for group_chains in groups.values() {
        let sym = &group_chains[0][depth];
        let is_last_in_chain = group_chains.iter().all(|c| depth + 1 >= c.len());

        render_into_symbol(out, sym, children_set, &indent);

        if is_last_in_chain {
            // This is a target (last segment match). Show children.
            if expand_children {
                let child_indent = format!("{indent}\t");
                render_target_children(
                    out,
                    sym,
                    children_set,
                    symbol_index,
                    file_path,
                    &child_indent,
                );
            }
        } else {
            // Recurse to the next depth level.
            let sub_chains: Vec<&Vec<Symbol>> = group_chains
                .iter()
                .filter(|c| depth + 1 < c.len())
                .copied()
                .collect();
            let owned: Vec<Vec<Symbol>> = sub_chains.iter().map(|c| (*c).clone()).collect();
            render_chain_tree(
                out,
                &owned,
                depth + 1,
                children_set,
                expand_children,
                symbol_index,
                file_path,
                indent_level + 1,
            );
        }
    }
}

/// Renders a single symbol line for `into` output.
fn render_into_symbol(
    out: &mut String,
    sym: &Symbol,
    children_set: &HashSet<String>,
    indent: &str,
) {
    let kind_label = format_symbol_kind(&sym.kind);
    let trailing = if children_set.contains(&sym.name) {
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

/// Renders the children of a target symbol (or "no nested definitions").
fn render_target_children(
    out: &mut String,
    target: &Symbol,
    children_set: &HashSet<String>,
    symbol_index: &SymbolIndex,
    file_path: &Path,
    indent: &str,
) {
    if !children_set.contains(&target.name) {
        // Leaf symbol — no nested definitions.
        let _ = writeln!(
            out,
            "{indent}(no nested definitions \u{2014} read :{}-{})",
            target.line + 1,
            target.end_line + 1,
        );
        return;
    }

    // Query children of this target.
    let children = symbol_index
        .query_scoped(
            &[file_path],
            &ScopeFilter::ChildrenOf(&target.name),
            "*",
            None,
            false,
        )
        .ok()
        .and_then(|m| m.get(&file_path.to_path_buf()).cloned())
        .unwrap_or_default();

    for child in &children {
        render_into_symbol(out, child, children_set, indent);
    }
}

/// Queries symbols with `{a,b}` alternation support.
///
/// `SQLite` GLOB doesn't support `{a,b}` syntax. This function expands
/// alternation patterns and merges results from multiple queries.
fn query_with_alternation(
    symbol_index: &SymbolIndex,
    files: &[&Path],
    scope: &ScopeFilter<'_>,
    name_pattern: &str,
    kind_filter: Option<&str>,
    deprecated_only: bool,
) -> Result<HashMap<PathBuf, Vec<Symbol>>> {
    let patterns = expand_alternation(name_pattern);

    if patterns.len() == 1 {
        return symbol_index.query_scoped(files, scope, &patterns[0], kind_filter, deprecated_only);
    }

    let mut merged: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
    let mut seen: HashSet<(PathBuf, u32)> = HashSet::new();

    for pat in &patterns {
        let results = symbol_index.query_scoped(files, scope, pat, kind_filter, deprecated_only)?;
        for (path, syms) in results {
            let entry = merged.entry(path.clone()).or_default();
            for sym in syms {
                if seen.insert((path.clone(), sym.line)) {
                    entry.push(sym);
                }
            }
        }
    }

    // Sort by line within each file.
    for syms in merged.values_mut() {
        syms.sort_by_key(|s| s.line);
    }

    Ok(merged)
}

/// Builds a children set (names that appear as scope) for a single file.
fn build_children_set_for_file(symbol_index: &SymbolIndex, path: &Path) -> HashSet<String> {
    symbol_index
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
        .unwrap_or_default()
}

// ─── Bucketed rendering ──────────────────────────────────────────────

/// Renders bucketed output from entry names.
fn render_bucketed(file_names: &[String], budget: usize) -> String {
    if file_names.is_empty() {
        return String::new();
    }

    let bucket_input: Vec<BucketEntry> = file_names
        .iter()
        .map(|name| BucketEntry {
            value: name.clone(),
            context: None,
        })
        .collect();

    let buckets = bucketing::bucket(&bucket_input, budget, false);

    let mut result = String::new();
    for b in &buckets {
        if b.count == 1 {
            if let Some(ref entries) = b.entries {
                if let Some(entry) = entries.first() {
                    let _ = writeln!(result, "{}", entry.value);
                }
            } else {
                let _ = writeln!(result, "{}", b.pattern);
            }
        } else {
            let _ = writeln!(result, "{}  ({} files)", b.pattern, b.count);
        }
    }

    result
}

/// Renders directory + count tier: groups matched entries by their
/// immediate parent directory and emits `dir/\tcount`.
///
/// Entries without a parent component (top-level files/dirs) are listed
/// individually. Directory entries (trailing `/`) count as entries in
/// their parent.
fn render_dir_count(rel_paths: &[String]) -> String {
    if rel_paths.is_empty() {
        return String::new();
    }

    let mut dir_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut top_level: Vec<String> = Vec::new();

    for rel in rel_paths {
        if let Some(pos) = rel.rfind('/') {
            // Strip trailing slash from directory entries before splitting.
            let effective = rel.strip_suffix('/').unwrap_or(rel);
            if let Some(dir_pos) = effective.rfind('/') {
                let dir = &effective[..dir_pos];
                *dir_counts.entry(format!("{dir}/")).or_insert(0) += 1;
            } else if pos == rel.len() - 1 {
                // Top-level directory entry like "subdir/"
                top_level.push(rel.clone());
            } else {
                // File in a subdirectory at depth 1
                let dir = &rel[..pos];
                *dir_counts.entry(format!("{dir}/")).or_insert(0) += 1;
            }
        } else {
            // Top-level file
            top_level.push(rel.clone());
        }
    }

    let mut result = String::new();
    for name in &top_level {
        let _ = writeln!(result, "{name}");
    }
    for (dir, count) in &dir_counts {
        let _ = writeln!(result, "{dir}\t{count}");
    }
    result
}

// ─── Root-grouped assembly ───────────────────────────────────────────

/// Per-root tree and metadata for glob pattern tier selection.
struct GlobSection {
    root: PathBuf,
    node: DirNode,
    flat_names: Vec<String>,
    files: Vec<(PathBuf, PathBuf, bool)>,
}

// ─── Cursor-based paging ──────────────────────────────────────────────

/// Encodes a cursor token from a 0-based line number.
fn encode_cursor(line: u32) -> String {
    format!("g{line}")
}

/// Decodes a cursor token to a 0-based line number.
fn decode_cursor(token: &str) -> Result<u32> {
    token
        .strip_prefix('g')
        .ok_or_else(|| anyhow!("invalid cursor token"))
        .and_then(|s| {
            s.parse::<u32>()
                .map_err(|_| anyhow!("invalid cursor token"))
        })
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
        let root = PathBuf::from("/test/root");
        let mut node = DirNode::new();

        // Insert two files at root level.
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

        let sections = vec![GlobSection {
            root: root.clone(),
            node,
            flat_names: vec!["main.rs".to_string(), "lib.rs".to_string()],
            files: vec![
                (PathBuf::from("/test/root/main.rs"), root.clone(), false),
                (PathBuf::from("/test/root/lib.rs"), root, false),
            ],
        }];

        // Render tier 2.
        let mut tier2 = String::new();
        for s in &sections {
            if !tier2.is_empty() {
                tier2.push('\n');
            }
            let _ = writeln!(tier2, "Root: {}", s.root.display());
            s.node.render_tier2(
                &mut tier2,
                0,
                None,
                200,
                &[],
                // Use a minimal FilesystemManager — render_tier2 only uses
                // it for outline_suppress checking, which we pass empty.
                &FilesystemManager::new(),
            );
        }

        assert!(
            tier2.contains("main.rs"),
            "tier2 should contain main.rs: {tier2}"
        );
        assert!(
            tier2.contains("lib.rs"),
            "tier2 should contain lib.rs: {tier2}"
        );
        assert!(
            tier2.contains("Root: /test/root"),
            "tier2 should contain Root header: {tier2}"
        );
    }

    #[test]
    fn test_render_dir_count_basic() {
        let paths = vec![
            "dirA/file1.rs".to_string(),
            "dirA/file2.rs".to_string(),
            "dirB/file1.rs".to_string(),
        ];
        let result = render_dir_count(&paths);
        assert!(
            result.contains("dirA/\t2"),
            "should show dirA with count 2: {result}"
        );
        assert!(
            result.contains("dirB/\t1"),
            "should show dirB with count 1: {result}"
        );
    }

    #[test]
    fn test_render_dir_count_top_level() {
        let paths = vec!["top.rs".to_string(), "dirA/nested.rs".to_string()];
        let result = render_dir_count(&paths);
        assert!(
            result.contains("top.rs"),
            "should list top-level file: {result}"
        );
        assert!(
            result.contains("dirA/\t1"),
            "should show dirA with count: {result}"
        );
    }

    #[test]
    fn test_render_dir_count_dir_entries() {
        // Directory entries (trailing /) at top level are listed individually.
        let paths = vec!["subdir/".to_string(), "another/".to_string()];
        let result = render_dir_count(&paths);
        assert!(result.contains("subdir/"), "should list subdir: {result}");
        assert!(result.contains("another/"), "should list another: {result}");
    }

    #[test]
    fn test_render_dir_count_empty() {
        let result = render_dir_count(&[]);
        assert!(result.is_empty(), "empty input should produce empty output");
    }

    #[test]
    fn test_render_dir_count_nested() {
        let paths = vec![
            "a/b/file1.rs".to_string(),
            "a/b/file2.rs".to_string(),
            "a/c/file3.rs".to_string(),
        ];
        let result = render_dir_count(&paths);
        assert!(
            result.contains("a/b/\t2"),
            "should group nested dir: {result}"
        );
        assert!(
            result.contains("a/c/\t1"),
            "should group nested dir: {result}"
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
}
