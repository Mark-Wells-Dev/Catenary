// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Glob tool handler: the structural read.
//!
//! glob is grep's counterpart: where grep enriches a *hit* by where it lives,
//! glob enriches a *file* by what's in it. Each path is dispatched by type:
//! - File path → header `path  (N lines)` + the file's **fully-expanded**
//!   `documentSymbol` outline, one node per line, re-indented by tree depth.
//! - Directory path → its immediate entries (one level, not recursive): files
//!   get their outline; subdirectories get `name/  (N files, M dirs)` — the
//!   immediate child counts that track the active flags, a preview of the next
//!   glob.
//!
//! **Enrich always.** Every file with symbols is outlined and every directory
//! shows its child counts — there is no per-file size gate and no file-count
//! gate (both were intent-guesses that shave the wrong end). A file whose
//! language has no server, or whose `documentSymbol` fails, is listed with a
//! `no outline` marker rather than silently outline-less. The kind is implicit
//! in each declaration source line — no `SymbolKind` ever surfaces.
//!
//! The output is always complete (decision 025): the full outline prints, with
//! no volume branch — the host caps only the final read at the end of a pipeline.

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
use super::session::{ArgResolution, ResolvedGlob, expand_search_paths_grouped_cancellable};
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
    /// Original argument spellings, as the agent typed them, 1:1 with `paths`.
    ///
    /// `paths` holds the cwd-absolutized forms the pipeline expands;
    /// `display_paths` preserves the pre-resolution spelling so a glob pattern's
    /// cardinality header (misc 121) echoes what the agent typed — e.g.
    /// `src/lsp/*.rs`, not its absolute form — matching the zero-match report's
    /// original-spelling contract (misc 118). Empty when a caller builds params
    /// without spellings; the header then falls back to the absolute pattern.
    #[serde(default)]
    pub display_paths: Vec<String>,
    /// Return a path count instead of rendered results (default: false).
    ///
    /// Short-circuits LSP enrichment: the pipeline reports the number of
    /// resolved filesystem paths, never a rendered tree.
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
/// Normal queries render the complete tree to stdout; `--count`
/// (`GlobInput::count`) short-circuits to a path count.
pub enum GlobOutcome {
    /// The complete rendered tree output for stdout.
    Rendered {
        /// The complete output for stdout.
        output: String,
        /// Indices (into the request's `paths`) of glob-pattern arguments that
        /// expanded to zero matches. The daemon reports these positionally so
        /// the CLI can render a loud per-argument
        /// `no matches for pattern: <pattern>` line against the *original*
        /// argument spelling (misc 118).
        no_match_indices: Vec<usize>,
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
    /// Glob patterns whose outlines are suppressed from automatic display (an
    /// explicit user opt-out, e.g. generated/vendored files — not an
    /// intent-guess gate). Symbols remain available; the entry is flagged
    /// `[symbols available]` instead of outlined.
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
        cancel: &tokio_util::sync::CancellationToken,
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

        // Count mode short-circuits enrichment — report the number of resolved
        // paths, not a rendered tree. The count walks run off the runtime thread
        // so a massive directory is cancellable mid-walk (misc 140 phase 2).
        if input.count {
            let paths = self
                .count_paths_off_thread(
                    input.paths.clone(),
                    input.include_gitignored,
                    input.include_hidden,
                    exclude.clone(),
                    cancel,
                )
                .await?;
            return Ok(GlobOutcome::Count { paths });
        }

        // cwd-scoped search: present when the original pattern was relative.
        let cwd = input.cwd.as_deref();

        // Run pipeline — handlers return the complete output. Existing paths
        // dispatch directly; unexpanded glob patterns are expanded daemon-side.
        // The output is always complete (decision 025): the full outline prints,
        // with no volume branch — the host caps only the final read. `cancel` is
        // threaded into the directory walk so a disconnected client's listing
        // stops promptly instead of running to completion (misc 140).
        let (output, no_match_indices) = self
            .handle_literal_paths(
                &input.paths,
                &input,
                exclude.as_ref(),
                cwd,
                parent_id,
                cancel,
            )
            .await?;

        Ok(GlobOutcome::Rendered {
            output,
            no_match_indices,
        })
    }

    /// Single file: header `path  (N lines)` + its fully-expanded outline.
    ///
    /// Enrich always — the file is outlined whenever it has symbols (the
    /// `outline_threshold` size gate is gone). A file whose language has no
    /// server, or whose `documentSymbol` returned nothing, carries the
    /// `no outline` degradation marker; a file matched by `outline_suppress`
    /// keeps its `[symbols available]` flag in place of the body. Returns the
    /// complete output.
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

        let line_count = metadata
            .as_ref()
            .and_then(|m| self.fs_manager.line_count(path, m));

        // Resolve the outline once: whether the file has symbols, whether it is
        // suppressed, and (when it will be rendered) the symbols themselves.
        let suppressed = is_outline_suppressed(path, &self.outline_suppress, &self.fs_manager);
        let (has_symbols, syms) = self
            .symbol_index
            .as_ref()
            .and_then(|arc| arc.lock().ok())
            .map_or((false, None), |idx| {
                let hs = idx.has_symbols_for(path);
                let syms = if hs && !suppressed {
                    idx.query(".*", Some(std::slice::from_ref(&path.to_path_buf())))
                        .ok()
                        .map(|all| all.into_iter().map(|(_, s)| s).collect::<Vec<_>>())
                } else {
                    None
                };
                (hs, syms)
            });

        // Header: `(N lines)`, plus `no outline` when a text file has no
        // symbols, or `[symbols available]` when symbols exist but are
        // suppressed. Binary files report a size and are never marked.
        if let Some(lc) = line_count {
            let lines = pluralize_lines(lc);
            if has_symbols && suppressed {
                let _ = writeln!(full, "{display}  ({lines}) [symbols available]");
            } else if has_symbols {
                let _ = writeln!(full, "{display}  ({lines})");
            } else {
                let _ = writeln!(full, "{display}  ({lines}, no outline)");
            }
        } else {
            let size = metadata.map_or(0, |m| m.len());
            let _ = writeln!(full, "{display}  ({})", format_file_size(size));
        }

        if let Some(syms) = syms {
            let mut sources = SourceLines::new();
            render_full_outline(&mut full, path, &syms, "\t", &mut sources);
        }

        full
    }

    /// Dispatch each resolved path through the file or directory handler.
    ///
    /// Paths that exist are dispatched directly; non-existent paths are treated
    /// as glob patterns and expanded daemon-side via the gitignore-aware
    /// `ignore` walker ([`expand_search_paths`](super::session::expand_search_paths)).
    /// A shell-expanded (unquoted)
    /// glob arrives as concrete paths and an unexpanded (quoted) glob arrives
    /// as a pattern — both resolve to the same set here.
    ///
    /// Returns the rendered output plus the indices (into `paths`) of
    /// glob-pattern arguments that expanded to zero matches — the CLI turns
    /// these into a loud per-argument `no matches for pattern` report (misc
    /// 118).
    ///
    /// A glob-pattern argument that matched **≥1** path opens with a one-line
    /// cardinality header — `N files match <pattern>` (singular grammar for one)
    /// — printed *before* that pattern's per-file listings, so a
    /// `| head`-truncated view still shows the true count (misc 121). The header
    /// uses the pattern's original spelling ([`GlobInput::display_paths`]),
    /// matching the zero-match report. Directory and single-file arguments
    /// render unchanged — a directory already shows its own structure and a
    /// named file is its own answer.
    async fn handle_literal_paths(
        &self,
        paths: &[PathBuf],
        input: &GlobInput,
        exclude: Option<&ResolvedGlob>,
        cwd: Option<&Path>,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(String, Vec<usize>)> {
        // Per-argument resolution so each pattern's matches stay grouped for its
        // cardinality header; the flat set drives the nudge, exactly as before.
        // The pattern expansion runs off the runtime thread so a massive pattern
        // base directory is cancellable mid-walk (misc 140 phase 2).
        let mut groups = {
            let paths = paths.to_vec();
            let include_gitignored = input.include_gitignored;
            let include_hidden = input.include_hidden;
            let cancel = cancel.clone();
            tokio::task::spawn_blocking(move || {
                expand_search_paths_grouped_cancellable(
                    &paths,
                    include_gitignored,
                    include_hidden,
                    &cancel,
                )
            })
            .await
            .map_err(|e| anyhow!("glob path expansion task failed: {e}"))?
        };

        // Apply the compiled exclude to each glob pattern's matches so a pattern
        // argument honors `--exclude-pattern` exactly as a named-directory
        // argument does (bug 73). Filtering here — before the flat set feeds the
        // scoped nudge and before the render loop — keeps the listing, the nudge,
        // and the `--count` leg (which filters identically) in agreement, and
        // leaves a pattern whose every match is excluded with an empty
        // `resolved`, so it falls through to the honest `no matches for pattern`
        // report below rather than vanishing.
        apply_exclude_to_groups(&self.fs_manager, &mut groups, exclude);

        let resolved: Vec<PathBuf> = groups
            .iter()
            .flat_map(|g| g.resolved.iter().cloned())
            .collect();

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
        let mut no_match_indices = Vec::new();
        for (i, group) in groups.iter().enumerate() {
            // Real walk cancellation (misc 140): the router fires this token when
            // the CLI client disconnects. Between paths — after the awaits above
            // and each path's own LSP round-trips — a fired token stops the
            // listing before the next path. A cancelled walk yields no partial
            // response (the router already returned); this just ends the work.
            if cancel.is_cancelled() {
                break;
            }
            if group.is_pattern {
                if group.resolved.is_empty() {
                    // A pattern that matched nothing is reported loudly CLI-side
                    // (misc 118); nothing renders here.
                    no_match_indices.push(i);
                    continue;
                }
                // A pattern with ≥1 match opens with its cardinality header, so a
                // `| head`-truncated view still shows the true count (misc 121).
                // Echo the original spelling (falling back to the absolute
                // pattern when a caller supplied no `display_paths`).
                let display = input
                    .display_paths
                    .get(i)
                    .cloned()
                    .or_else(|| paths.get(i).map(|p| p.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                let _ = writeln!(
                    full,
                    "{}",
                    match_count_header(group.resolved.len(), &display)
                );
            }
            for path in &group.resolved {
                // Directories first — `is_dir()` follows symlinks, so a
                // symlink-to-dir lists its contents (rather than rendering as a
                // single file header). This dir-first order matches
                // `collect_scoped_observations` so the listing and the changed-set
                // nudge classify a symlink-to-dir the same way (WS31-review walk-2).
                if path.is_dir() {
                    let output = self
                        .handle_glob_dir(path, input, exclude, cwd, parent_id, cancel)
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
        }
        Ok((full, no_match_indices))
    }

    /// Routes glob's scoped changed-set nudge (WS31 ticket 04,
    /// [`WalkBreadth::Scoped`](crate::lsp::WalkBreadth::Scoped)).
    ///
    /// `resolved` is the glob pattern's resolved path set (from
    /// [`expand_search_paths`](super::session::expand_search_paths)). The breadth
    /// of a glob walk is exactly the
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

    /// Directory listing: the one-level structural read.
    ///
    /// Collects immediate children, applies visibility and exclude filters, and
    /// renders them: files get their fully-expanded outline (or the `no outline`
    /// marker), subdirectories get `name/  (N files, M dirs)` immediate child
    /// counts (no recursion). The directory's own header carries the same
    /// `(N files, M dirs)` count — a directory renders identically as a child
    /// entry and as the glob target's header — so an empty directory is
    /// `path/  (empty)`. Returns the complete output.
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
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

        let entries = self
            .collect_dir_entries_off_thread(
                canonical.clone(),
                input.include_gitignored,
                input.include_hidden,
                exclude.cloned(),
                cancel,
            )
            .await?;

        // The target's own count = its immediate entries (what this glob
        // enumerated), split into files and directories — the same split a
        // subdir child entry shows, so the two forms agree.
        let target_files = entries.iter().filter(|e| !e.is_dir).count();
        let target_dirs = entries.iter().filter(|e| e.is_dir).count();
        let target_suffix = dir_count_suffix(target_files, target_dirs);

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
            let _ = writeln!(full, "{display}/  {target_suffix}");
        } else {
            // Absolute pattern outside workspace roots: LSP warning.
            if self.fs_manager.resolve_root(&canonical).is_none() {
                let _ = writeln!(full, "{NO_LSP_LABEL}");
            }
            let _ = writeln!(full, "{}/  {target_suffix}", canonical.display());
        }

        if entries.is_empty() {
            return Ok(full);
        }

        // Populate the symbol index for every listed file (enrich always).
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

        let content = render_dir(
            &entries,
            ts_guard.as_deref(),
            &self.outline_suppress,
            &self.fs_manager,
            "\t",
            input,
            exclude,
        );
        full.push_str(&content);
        Ok(full)
    }

    /// Extracts file info: `(line_count, binary_size)`.
    ///
    /// A free helper over [`FilesystemManager`] (not `&self`) so the directory
    /// walk that calls it can run off the runtime thread in a `spawn_blocking`
    /// task (misc 140 phase 2).
    fn file_info(
        fs_manager: &FilesystemManager,
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> (Option<usize>, Option<String>) {
        metadata.map_or((None, None), |m| {
            fs_manager.line_count(path, m).map_or_else(
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
    ///
    /// `cancel` is checked per entry: a fired token (the CLI client disconnected)
    /// stops the one-level enumeration so a directory of many children is not read
    /// to completion for a client that is gone (misc 140). The walk is
    /// `max_depth(1)`, so this bounds a single wide directory rather than a
    /// recursive tree.
    ///
    /// A free helper over [`FilesystemManager`] (not `&self`) so
    /// [`Self::collect_dir_entries_off_thread`] can run it in a `spawn_blocking`
    /// task — a single massive directory is then cancellable mid-walk once off
    /// the runtime thread (misc 140 phase 2).
    #[allow(clippy::too_many_lines, reason = "sequential per-entry classification")]
    fn collect_dir_entries(
        fs_manager: &FilesystemManager,
        canonical: &Path,
        include_gitignored: bool,
        include_hidden: bool,
        exclude: Option<&ResolvedGlob>,
        cancel: &tokio_util::sync::CancellationToken,
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
            if let Some(rg) = exclude
                && rg.is_match(&entry_path, canonical)
            {
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
                    Self::file_info(fs_manager, &entry_path, resolved_meta.as_ref())
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
                    Self::file_info(fs_manager, &entry_path, Some(&metadata))
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

    /// Runs [`Self::collect_dir_entries`] on a blocking thread (misc 140 phase 2).
    ///
    /// The one-level directory enumeration is synchronous; left on an async
    /// runtime worker it would pin that thread, so the router's disconnect
    /// `select!` could never poll its cancel branch — a single massive directory
    /// would be read to completion for a dead client. `spawn_blocking` frees the
    /// runtime to fire the cancel token, which the walk observes per entry
    /// (mirroring grep's `ripgrep_matches_blocking`). Owned inputs move into the
    /// task; `GlobEntry`/`ResolvedGlob` are `Send`.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata reads fail or the blocking task panics.
    async fn collect_dir_entries_off_thread(
        &self,
        canonical: PathBuf,
        include_gitignored: bool,
        include_hidden: bool,
        exclude: Option<ResolvedGlob>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<GlobEntry>> {
        let fs_manager = Arc::clone(&self.fs_manager);
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            Self::collect_dir_entries(
                &fs_manager,
                &canonical,
                include_gitignored,
                include_hidden,
                exclude.as_ref(),
                &cancel,
            )
        })
        .await
        .map_err(|e| anyhow!("glob directory walk task failed: {e}"))?
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
    ///
    /// A free helper over [`FilesystemManager`] (not `&self`) so
    /// [`Self::count_paths_off_thread`] can run it in a `spawn_blocking` task —
    /// the pattern expansion and per-directory walks are then cancellable
    /// mid-walk once off the runtime thread (misc 140 phase 2). Every walk is the
    /// cancellable form, so a fired token stops it promptly.
    fn count_paths(
        fs_manager: &FilesystemManager,
        paths: &[PathBuf],
        include_gitignored: bool,
        include_hidden: bool,
        exclude: Option<&ResolvedGlob>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<usize> {
        // Resolve per-argument so the exclude filter distinguishes a glob
        // pattern's matches (filtered) from a directly-named path (never filtered
        // — a named directory's entries are filtered downstream). This mirrors
        // the rendered listing exactly, so `--count` and the listing agree under
        // an exclude (bug 73).
        let mut groups = expand_search_paths_grouped_cancellable(
            paths,
            include_gitignored,
            include_hidden,
            cancel,
        );
        apply_exclude_to_groups(fs_manager, &mut groups, exclude);
        let resolved: Vec<PathBuf> = groups.into_iter().flat_map(|g| g.resolved).collect();
        let mut total = 0usize;
        for path in &resolved {
            if cancel.is_cancelled() {
                break;
            }
            if path.is_dir() {
                let canonical = path
                    .canonicalize()
                    .map_err(|e| anyhow!("Path does not exist: {}: {e}", path.display()))?;
                total += Self::collect_dir_entries(
                    fs_manager,
                    &canonical,
                    include_gitignored,
                    include_hidden,
                    exclude,
                    cancel,
                )?
                .len();
            } else if path.is_file() || path.is_symlink() {
                total += 1;
            }
        }
        Ok(total)
    }

    /// Runs [`Self::count_paths`] on a blocking thread (misc 140 phase 2).
    ///
    /// Same rationale as [`Self::collect_dir_entries_off_thread`]: the count walks
    /// (pattern expansion + per-directory enumeration) are synchronous, so
    /// `spawn_blocking` keeps the router's disconnect `select!` pollable and lets
    /// the cancel token actually fire mid-walk.
    ///
    /// # Errors
    ///
    /// Returns an error if a path cannot be canonicalized or the blocking task
    /// panics.
    async fn count_paths_off_thread(
        &self,
        paths: Vec<PathBuf>,
        include_gitignored: bool,
        include_hidden: bool,
        exclude: Option<ResolvedGlob>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<usize> {
        let fs_manager = Arc::clone(&self.fs_manager);
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            Self::count_paths(
                &fs_manager,
                &paths,
                include_gitignored,
                include_hidden,
                exclude.as_ref(),
                &cancel,
            )
        })
        .await
        .map_err(|e| anyhow!("glob count walk task failed: {e}"))?
    }
}

// ─── Glob pattern cardinality header ──────────────────────────────────

/// Formats a glob pattern's cardinality header — one line, printed before the
/// pattern's per-file listings so a `| head`-truncated view still shows the
/// true count (misc 121). Singular grammar for a lone match: `1 file matches
/// <pattern>`; plural otherwise: `N files match <pattern>`. `count` is always
/// ≥1 (a zero-match pattern renders the `no matches for pattern` report
/// instead).
fn match_count_header(count: usize, display: &str) -> String {
    let (noun, verb) = if count == 1 {
        ("file", "matches")
    } else {
        ("files", "match")
    };
    format!("{count} {noun} {verb} {display}")
}

// ─── Outline eligibility ──────────────────────────────────────────────

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
fn render_symbol_line(out: &mut String, sym: &Symbol, indent: &str, source: Option<&str>) {
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
fn render_full_outline(
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
/// [`GlobServer::collect_dir_entries`]), so previewing a huge `target/` does not
/// read 40 000 files. Symlinks are followed for the file/dir decision, matching
/// the listing's classification.
fn count_dir_children(
    dir: &Path,
    input: &GlobInput,
    exclude: Option<&ResolvedGlob>,
) -> (usize, usize) {
    let walker = WalkBuilder::new(dir)
        .max_depth(Some(1))
        .git_ignore(!input.include_gitignored)
        .hidden(!input.include_hidden)
        .build();
    let mut files = 0usize;
    let mut dirs = 0usize;
    for entry in walker.flatten() {
        let entry_path = entry.into_path();
        if entry_path.as_path() == dir {
            continue;
        }
        if let Some(rg) = exclude
            && rg.is_match(&entry_path, dir)
        {
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
/// ([`GlobServer::collect_dir_entries`]) and count walks, so both argument kinds
/// surface the same surviving set. A no-op when `exclude` is `None`.
///
/// Shared by [`GlobServer::handle_literal_paths`] (rendered listing) and
/// [`GlobServer::count_paths`] (`--count`) so the two never diverge, and applied
/// before the flat set feeds glob's scoped nudge so the nudge tracks only the
/// files the query surfaces. A pattern whose every match is excluded is left
/// with an empty `resolved`, so it renders the honest `no matches for pattern`
/// report rather than vanishing.
fn apply_exclude_to_groups(
    fs_manager: &FilesystemManager,
    groups: &mut [ArgResolution],
    exclude: Option<&ResolvedGlob>,
) {
    let Some(rg) = exclude else {
        return;
    };
    for group in groups.iter_mut().filter(|g| g.is_pattern) {
        group
            .resolved
            .retain(|path| !path_matches_exclude(fs_manager, rg, path));
    }
}

/// Whether `exclude` selects `path`, resolving the root a relative pattern
/// strips.
///
/// An absolute exclude matches the full path (the root is ignored); a basename
/// exclude (the `**/<name>` form the router expands a no-slash pattern into) is
/// root-relative, so the path's owning workspace root is stripped first —
/// falling back to the path's parent, then the path itself, when no root owns
/// it. `**/<name>` is depth-independent, so any ancestor root yields the same
/// verdict. This mirrors the entry-level filter
/// [`GlobServer::collect_dir_entries`] and the grep walk apply, so a
/// pattern-matched path is excluded on the same terms as a directory entry.
fn path_matches_exclude(
    fs_manager: &FilesystemManager,
    exclude: &ResolvedGlob,
    path: &Path,
) -> bool {
    let root = fs_manager
        .resolve_root(path)
        .or_else(|| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf());
    exclude.is_match(path, &root)
}

// ─── Directory rendering ─────────────────────────────────────────────

/// Renders a directory listing: subdirectories with their child counts, then
/// files with their fully-expanded outlines.
///
/// Enrich always — every file with symbols is outlined (no size/count gate),
/// unless matched by `outline_suppress` (which keeps a `[symbols available]`
/// flag in its place). A file whose language has no server, or whose
/// `documentSymbol` produced nothing, carries the `no outline` marker. Each
/// subdirectory shows `name/  (N files, M dirs)` — its immediate child counts
/// under the active flags, no recursion. Directories sort before files; both
/// sort by name. `symbol_index` is `None` only outside the daemon; otherwise it
/// is an index that may simply hold no symbols for an unserved file.
#[allow(clippy::too_many_lines, reason = "sequential rendering pipeline")]
fn render_dir(
    entries: &[GlobEntry],
    symbol_index: Option<&SymbolIndex>,
    outline_suppress: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
    indent: &str,
    input: &GlobInput,
    exclude: Option<&ResolvedGlob>,
) -> String {
    // Outline nodes sit one level deeper than their file header.
    let sym_base_indent = format!("{indent}\t");
    let mut sources = SourceLines::new();
    let mut result = String::new();

    // Subdirectories first (sorted), each with its immediate child counts.
    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    for d in &dirs {
        let (files, subdirs) = count_dir_children(&d.abs_path, input, exclude);
        let descriptor = dir_count_suffix(files, subdirs);
        let flags = compute_entry_flags(false, false, d.is_gitignored);
        render_entry_line(&mut result, d, &descriptor, &flags, indent);
    }

    // Files (sorted), each with its fully-expanded outline (enrich always).
    let mut files: Vec<&GlobEntry> = entries.iter().filter(|e| !e.is_dir).collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    for f in &files {
        if f.is_broken_symlink || f.is_snapshot {
            let flags = compute_entry_flags(false, false, f.is_gitignored);
            render_entry_line(&mut result, f, "", &flags, indent);
            continue;
        }

        let has_symbols = has_symbols_available(&f.abs_path, symbol_index);
        let suppressed =
            has_symbols && is_outline_suppressed(&f.abs_path, outline_suppress, fs_manager);
        // A text file (it has a line count) with no symbols degraded — mark it.
        let mark_no_outline = f.line_count.is_some() && !has_symbols;
        let descriptor = file_descriptor(f, mark_no_outline);
        let flags = compute_entry_flags(has_symbols, suppressed, f.is_gitignored);
        render_entry_line(&mut result, f, &descriptor, &flags, indent);

        if has_symbols
            && !suppressed
            && let Some(idx) = symbol_index
            && let Ok(all) = idx.query(".*", Some(std::slice::from_ref(&f.abs_path)))
        {
            let syms: Vec<Symbol> = all.into_iter().map(|(_, s)| s).collect();
            render_full_outline(
                &mut result,
                &f.abs_path,
                &syms,
                &sym_base_indent,
                &mut sources,
            );
        }
    }

    result
}

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

    #[test]
    fn test_match_count_header() {
        // Singular grammar for a lone match; plural for the rest. `count` is
        // always ≥1 here (a zero-match pattern renders the loud report instead).
        assert_eq!(
            match_count_header(1, "src/lsp/glob.rs"),
            "1 file matches src/lsp/glob.rs"
        );
        assert_eq!(
            match_count_header(2, "src/lsp/*.rs"),
            "2 files match src/lsp/*.rs"
        );
        assert_eq!(
            match_count_header(42, "src/**/*.rs"),
            "42 files match src/**/*.rs"
        );
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
        let exclude = ResolvedGlob::new("**/*.rs").expect("compile exclude");

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

        apply_exclude_to_groups(&fs, &mut groups, Some(&exclude));

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
    fn apply_exclude_to_groups_none_is_noop() {
        let fs = FilesystemManager::new();
        let mut groups = vec![ArgResolution {
            resolved: vec![PathBuf::from("/root/a.rs")],
            is_pattern: true,
        }];
        apply_exclude_to_groups(&fs, &mut groups, None);
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
        let idx = SymbolIndex::new().expect("create index");
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

        let count = GlobServer::count_paths(
            &server.fs_manager,
            &input.paths,
            input.include_gitignored,
            input.include_hidden,
            None,
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("count_paths");

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

    // ─── zero-match pattern reporting (misc 118) ────────────────────

    /// `execute` surfaces the index of a glob-pattern argument that expanded to
    /// zero matches, so the CLI can report it loudly. A single unmatched pattern
    /// resolves to nothing (no dispatch, no LSP), so this stays fast and
    /// server-free.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn execute_reports_zero_match_pattern_index() {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("create tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");

        let server = test_glob_server();
        // A single absolute pattern whose base dir exists but matches no file.
        let params = serde_json::json!({
            "paths": [base.join("*.nomatch118").to_string_lossy()],
        });
        let cancel = tokio_util::sync::CancellationToken::new();
        let outcome = rt
            .block_on(server.execute(&params, None, &cancel))
            .expect("execute glob");

        let GlobOutcome::Rendered {
            output,
            no_match_indices,
        } = outcome
        else {
            unreachable!("a non-count glob yields Rendered");
        };
        assert!(
            output.is_empty(),
            "zero-match pattern renders nothing: {output:?}"
        );
        assert_eq!(
            no_match_indices,
            vec![0],
            "the sole pattern argument (index 0) is flagged as a no-match"
        );
    }

    // ─── off-thread directory walk cancellation (misc 140 phase 2) ──────

    /// A single massive directory is cancellable mid-walk once the enumeration
    /// runs off the runtime thread (`spawn_blocking`): a fired token quits the
    /// walk instead of reading every child. Mirrors grep's
    /// `ripgrep_matches_quits_when_token_fires` — the phase-1 pattern, now for
    /// glob's residual sync walk.
    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn collect_dir_entries_off_thread_quits_on_cancel() {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("create tempdir");
        let dir = tmp.path().canonicalize().expect("canonicalize");
        for i in 0..200 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x\n").expect("write child");
        }

        let server = test_glob_server();

        // Baseline: without cancellation the walk enumerates every child.
        let live = tokio_util::sync::CancellationToken::new();
        let full = rt
            .block_on(server.collect_dir_entries_off_thread(dir.clone(), false, false, None, &live))
            .expect("walk uncancelled");
        assert_eq!(full.len(), 200, "uncancelled walk lists every child");

        // A token fired before the walk quits it at the first entry — the walk
        // ran off-thread, so the token is actually observed.
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let cancelled = rt
            .block_on(server.collect_dir_entries_off_thread(dir, false, false, None, &cancel))
            .expect("walk cancelled");
        assert!(
            cancelled.is_empty(),
            "a fired token quits the off-thread directory walk (got {} entries)",
            cancelled.len(),
        );
    }
}
