// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grep tool: ripgrep + symbol index pipeline with LSP enrichment.

use super::session::ResolvedGlob;
use anyhow::{Result, anyhow};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, Sink, SinkMatch};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

use super::NO_LSP_LABEL;
use super::SourceLines;
use super::filesystem_manager::{
    FilesystemManager, OBSERVED_STAT_MISS_MTIME, mtime_nanos, stat_with_retry,
};
use super::handler::display_path;
use super::pagination::paginate;
use crate::config::DispatchMethod;
use crate::lsp::server::LspServer;
use crate::lsp::{LspClientManager, WalkBreadth};
use crate::source::Source;
use crate::symbol_index::{
    AtomId, Edge, EnrichmentKey, Symbol, SymbolEnrichment, SymbolIndex, Witness,
};

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
    /// Glob pattern to exclude from matches (optional).
    #[serde(default)]
    pub exclude: Option<String>,
    /// Include gitignored files (default: false).
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files (default: false).
    #[serde(default)]
    pub include_hidden: bool,
    /// Page number for paged results (default: 1).
    #[serde(default = "default_page")]
    pub page: usize,
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
}

/// Default page number for grep (1-based).
const fn default_page() -> usize {
    1
}

/// A classified hit from the grep pipeline.
struct GrepHit {
    file: PathBuf,
    line: u32,
    col: u32,
    /// The full source line at the hit (the one-atom payload, decision 024),
    /// verbatim and newline-stripped — not the matched token.
    matched_text: String,
    classification: HitClass,
}

/// Classification of a ripgrep hit against the symbol index.
enum HitClass {
    /// rg hit at a symbol index definition line — enriched with edges.
    Symbol { symbol: Symbol },
    /// rg hit at a non-definition line — a plain occurrence atom (no edges).
    /// In the one-atom model the line carries no enclosing tag.
    Reference,
    /// Symbol identified via `prepareRename` in a file with no symbol index
    /// data — a definition-like hit that still gets enriched.
    PrepareRenameSymbol,
}

/// Outcome of a grep query.
///
/// Normal queries render a paginated tree; `--count` (`GrepInput::count`)
/// short-circuits to a numeric summary instead of a page.
pub enum GrepOutcome {
    /// Rendered, paginated tree output.
    Rendered(String),
    /// `--count` summary: a dumb `grep -c`-style tally from the ripgrep pass.
    Count {
        /// Number of matching lines (a line with multiple matches counts
        /// once, like `grep -c`).
        matches: usize,
        /// Number of distinct files holding a match.
        files: usize,
    },
}

/// Grep tool server: ripgrep + symbol index pipeline with LSP enrichment.
pub struct GrepServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    pub(super) budget: usize,
    /// Single-slot result cache for sequential page fetches.
    pub(super) cache: std::sync::Mutex<super::result_cache::ResultCache>,
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
        use super::result_cache::{GrepCacheParams, cache_key};

        let input: GrepInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.pattern.is_empty() {
            return Err(anyhow!("pattern must be non-empty"));
        }

        if input.page == 0 {
            return Err(anyhow!("page must be >= 1"));
        }

        // Compute cache key from pipeline-affecting parameters.
        let key = cache_key(&GrepCacheParams {
            pattern: &input.pattern,
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
            && let Some(cached) = cache.get(key, input.page, &self.fs_manager)
        {
            return Ok(GrepOutcome::Rendered(cached));
        }

        // cwd-scoped search: present when no glob or relative glob.
        let cwd = input.cwd.clone();

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
                    }
                } else {
                    GrepOutcome::Rendered(String::new())
                });
            }
            expanded
        };

        // Count mode is a dumb, `grep -c`-style tally: a single ripgrep pass
        // over the whole pattern, no alternation split, no symbol
        // classification, no LSP. Matching lines (a line counts once) and the
        // distinct files holding them, straight from the ripgrep result.
        if input.count {
            return self.count_matches(&input, &search_paths, cwd.as_deref());
        }

        // Split top-level alternation into independent arms
        let arms = split_alternation(&input.pattern);

        let mut all_output = String::new();
        let mut touched: Vec<PathBuf> = Vec::new();
        for arm in &arms {
            let arm_input = GrepInput {
                pattern: arm.clone(),
                paths: search_paths.clone(),
                exclude: input.exclude.clone(),
                include_gitignored: input.include_gitignored,
                include_hidden: input.include_hidden,
                page: input.page,
                cwd: cwd.clone(),
                count: false,
            };
            let (output, witnesses) = self
                .run(arm_input, parent_id, cancel, cwd.as_deref())
                .await?;
            if !output.is_empty() {
                if !all_output.is_empty() {
                    all_output.push('\n');
                }
                all_output.push_str(&output);
            }
            touched.extend(witnesses);
        }

        if all_output.is_empty() {
            return Ok(GrepOutcome::Rendered(String::new()));
        }

        // Paginate first (borrows), then move output into cache. `touched` is the
        // union of witnesses across alternation arms — matched files (content)
        // and walked directories (membership) — so a host edit or a new matching
        // file invalidates the cache (bug #26 residual).
        touched.sort();
        touched.dedup();
        let paginated = paginate(&all_output, self.budget, input.page);
        let roots = self.client_manager.roots();
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(key, all_output, &roots, &touched, &self.fs_manager);
        }

        Ok(GrepOutcome::Rendered(paginated))
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
    fn count_matches(
        &self,
        input: &GrepInput,
        search_paths: &[PathBuf],
        cwd: Option<&Path>,
    ) -> Result<GrepOutcome> {
        let effective_roots = self.effective_search_roots(search_paths, cwd);
        let resolved_exclude = input
            .exclude
            .as_deref()
            .map(ResolvedGlob::new)
            .transpose()?
            .map(Arc::new);

        let rg = Self::ripgrep_matches(
            &input.pattern,
            &effective_roots,
            resolved_exclude.as_ref(),
            input.include_gitignored,
            input.include_hidden,
            &self.fs_manager,
        )?;

        let matches: usize = rg.file_line_texts.values().map(HashMap::len).sum();
        let files = rg.file_line_texts.len();
        Ok(GrepOutcome::Count { matches, files })
    }

    /// Grep pipeline: ripgrep + `documentSymbol` index + hit classification.
    #[allow(clippy::too_many_lines, reason = "Core grep orchestration")]
    async fn run(
        &self,
        input: GrepInput,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
        cwd: Option<&Path>,
    ) -> Result<(String, Vec<PathBuf>)> {
        debug!("Grep request: pattern={}", input.pattern);

        // All paths are literal — no glob interpretation. When no paths are
        // provided, bind to the invoking cwd (never another root) or, when the
        // caller has no cwd, the explicit all-roots mode. See
        // [`Self::effective_search_roots`] (bug 31).
        let effective_roots = self.effective_search_roots(&input.paths, cwd);
        let resolved_exclude = input
            .exclude
            .as_deref()
            .map(ResolvedGlob::new)
            .transpose()?
            .map(Arc::new);

        // Step 1: Ripgrep scoped to file set → raw hits with matched text.
        let rg = Self::ripgrep_matches(
            &input.pattern,
            &effective_roots,
            resolved_exclude.as_ref(),
            input.include_gitignored,
            input.include_hidden,
            &self.fs_manager,
        )?;

        if rg.file_lines.is_empty() {
            return Ok((String::new(), Vec::new()));
        }

        // Step 2: Ensure servers exist for matched files and wait for readiness.
        // `rg_paths` is also the cache's file-mtime snapshot set (the files
        // whose content the rendered output depends on).
        let rg_paths: Vec<PathBuf> = rg.file_lines.keys().map(PathBuf::from).collect();
        self.client_manager
            .ensure_and_wait_for_paths(&rg_paths)
            .await;

        // Collect dead languages so the pipeline can skip prepareRename for them.
        // Exclude single-file servers — they have no project context for
        // workspace-wide search. Checked after ensure_and_wait_for_paths so
        // servers are in a terminal state (Ready or Dead), not still initializing.
        let mut dead_languages: HashSet<String> = HashSet::new();
        let clients = self.client_manager.rooted_clients().await;
        for (key, client_mutex) in &clients {
            if !client_mutex.lock().await.is_alive() {
                debug!(
                    "[{}] server died \u{2014} tool will run in degraded mode",
                    key.language_id
                );
                dead_languages.insert(key.language_id.clone());
            }
        }

        // Step 2a: Route the changed-set nudge (WS31 Consumer A) under the
        // walk-breadth gate (ticket 04). Enriched `grep`'s reverse-direction
        // enrichment is whole-tree, so a covered root is a `Full` walk: the
        // ripgrep walk already statted every visited file, so the engine reuses
        // those observations (group by root, root-relative), diffs against the
        // per-root baseline, routes the delta per server, AND reaps deletions
        // (a baseline entry the full walk did not visit). A root with no
        // covering server is `WalkBreadth::None` — the `(no LSP)` case — and is
        // skipped entirely (no diff, no nudge). `--count` grep never reaches
        // `run`, so it pays nothing. No edited-set exclusion for grep.
        //
        // Reaping is gated per-root by whether the walk actually spanned the
        // whole registered root (WS31-review C1): a `Full` breadth only means a
        // covering server exists, not that the walk covered the root. A
        // path-scoped grep (`!input.paths.is_empty()`) or a pathless grep whose
        // cwd is a *subdir* of the root walked only a subtree, so it cannot
        // assert that an unvisited baseline entry is gone — it is add/update
        // only (`reap = false`), exactly like a scoped `glob`. Only a pathless
        // grep whose walked scope is an ancestor-or-equal of the registered root
        // reaps. The walked scopes are canonicalized so they compare against the
        // canonicalized roots `resolve_root` returns.
        {
            let mut by_root: HashMap<PathBuf, Vec<(PathBuf, i64)>> = HashMap::new();
            for (abs, mtime) in &rg.files {
                if let Some(root) = self.fs_manager.resolve_root(abs)
                    && let Ok(rel) = abs.strip_prefix(&root)
                {
                    by_root
                        .entry(root)
                        .or_default()
                        .push((rel.to_path_buf(), *mtime));
                }
            }
            // The scopes the walk actually covered, canonicalized to match the
            // canonical form of registered roots (see `run_daemon_main`).
            let walked_scopes: Vec<PathBuf> = effective_roots
                .iter()
                .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
                .collect();
            let no_exclude: HashSet<PathBuf> = HashSet::new();
            for (root, observed) in &by_root {
                let breadth = if self.client_manager.has_covering_watchers(root).await {
                    WalkBreadth::Full
                } else {
                    WalkBreadth::None
                };
                if !breadth.runs_engine() {
                    continue;
                }
                // Only reap when the walk truly covered the whole root: a
                // pathless grep whose walked scope is an ancestor-or-equal of
                // this registered root. A path-scoped grep, or a cwd below the
                // root, walked only a subtree → add/update only.
                let covered_whole_root = input.paths.is_empty()
                    && walked_scopes.iter().any(|scope| root.starts_with(scope));
                let reap = breadth.reaps() && covered_whole_root;
                self.client_manager
                    .nudge_changed_set(root, observed, &no_exclude, reap)
                    .await;
            }
        }

        // Step 2b: Populate (or refresh) symbol index for matched files.
        super::ensure_symbols(
            self.symbol_index.as_ref(),
            &self.client_manager,
            &self.fs_manager,
            &rg_paths,
            parent_id,
        )
        .await;

        // Step 3: Symbol index query.
        let (indexed_symbols, indexed_files) = if let Some(ref index_mutex) = self.symbol_index {
            let index = index_mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let re_pattern = format!("(?i){}", &input.pattern);
            let idx_syms = index
                .query(&re_pattern, Some(&rg_paths))
                .unwrap_or_default();
            let if_set: HashSet<String> = rg_paths
                .iter()
                .filter(|p| index.has_symbols_for(p))
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            drop(index);
            (idx_syms, if_set)
        } else {
            (Vec::new(), HashSet::new())
        };

        // Build lookup: definition atom id → Symbol.
        let mut def_lookup: HashMap<AtomId, Symbol> = HashMap::new();
        for (path, sym) in &indexed_symbols {
            def_lookup.insert(
                AtomId::new(path.to_string_lossy().as_ref(), sym.line),
                sym.clone(),
            );
        }

        // Step 4: Classify each rg hit.
        let mut hits: Vec<GrepHit> = Vec::new();

        for (file_str, line_map) in &rg.file_line_texts {
            let file_path = PathBuf::from(file_str);
            let has_symbols = indexed_files.contains(file_str);

            for (&line_1, texts) in line_map {
                let line_0 = line_1 - 1;
                let matched_text = texts.first().map(|(t, _)| t.clone()).unwrap_or_default();
                let col = texts.first().map_or(0, |(_, c)| *c);

                if has_symbols && let Some(sym) = def_lookup.get(&AtomId::new(file_str, line_0)) {
                    // Definition line in an indexed file → enriched symbol atom.
                    hits.push(GrepHit {
                        file: file_path.clone(),
                        line: line_0,
                        col,
                        matched_text,
                        classification: HitClass::Symbol {
                            symbol: sym.clone(),
                        },
                    });
                    continue;
                }

                // Non-definition line. No ripgrep match is ever dropped
                // (decision 024: `catenary grep` is a strict superset of `grep`
                // — every byte-match is rendered verbatim). `prepareRename`
                // gates *enrichment* only, never membership (bug 47).
                //
                // In an indexed file every non-definition hit is a plain
                // reference atom regardless of `prepareRename` (the enclosing
                // definition is a separate, enriched `Symbol`), so the round
                // trip is skipped entirely — one LSP call saved per hit. In a
                // non-indexed file `prepareRename` chooses enrich-vs-plain: a
                // confirmed symbol becomes an enriched `PrepareRenameSymbol`; a
                // non-symbol (a keyword, or prose body text on a Lattice /
                // markdown root, where only headings are renameable) renders as
                // a plain reference atom — present, just not enriched. The check
                // is also skipped when the server is dead.
                let classification = if has_symbols {
                    HitClass::Reference
                } else {
                    let lang = self.fs_manager.language_id(&file_path);
                    let server_dead = lang
                        .as_ref()
                        .is_some_and(|l| dead_languages.contains(l.as_str()));
                    let is_symbol = if server_dead {
                        // No live server to enrich with — a plain atom.
                        false
                    } else {
                        self.prepare_rename_check(&file_path, line_0, col, parent_id, cancel)
                            .await
                    };
                    if is_symbol {
                        // A confirmed symbol in a file the index has no grammar
                        // for: a definition-like atom enriched with edges.
                        HitClass::PrepareRenameSymbol
                    } else {
                        // Not a symbol — a verbatim reference atom, never dropped.
                        HitClass::Reference
                    }
                };
                hits.push(GrepHit {
                    file: file_path.clone(),
                    line: line_0,
                    col,
                    matched_text,
                    classification,
                });
            }
        }

        if hits.is_empty() {
            return Ok((String::new(), Vec::new()));
        }

        // Enrich definition-like hits (Symbol, PrepareRenameSymbol).
        // Reference hits pass through with no enrichment.
        let mut enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> = Vec::new();
        for hit in &hits {
            let (line_0, col) = match &hit.classification {
                HitClass::Symbol { symbol } => (symbol.line, hit.col),
                HitClass::PrepareRenameSymbol => (hit.line, hit.col),
                HitClass::Reference => {
                    enrichments.push((hit, None));
                    continue;
                }
            };
            if cancel.is_cancelled() {
                return Err(crate::mcp::RequestCancelled.into());
            }
            let enrichment = self
                .enrich_at_position(&hit.file, line_0, col, parent_id, cancel)
                .await;
            enrichments.push((hit, enrichment));
        }

        let rendered = render_results(&enrichments, &self.fs_manager, cwd);
        // Witnesses = matched files (content) + every directory the output's
        // membership depends on: the search roots and the subdirectories the
        // walk descended into. A file added directly to a root bumps the root's
        // mtime; one added deeper bumps its parent (which the walk visited). The
        // walker does not reliably surface the root entry itself, so include the
        // roots explicitly.
        let mut witnesses = rg_paths;
        witnesses.extend(rg.dirs);
        witnesses.extend(effective_roots);
        Ok((rendered, witnesses))
    }

    /// Checks `prepareRename` at a position to decide whether a hit is a
    /// renameable symbol worth enriching — an *enrichment* signal only, never a
    /// filter (bug 47). A `false` answer demotes the hit to a plain reference
    /// atom; it never drops it.
    ///
    /// Uses priority chain dispatch: iterates servers that support rename
    /// in binding order, returns on the first definitive answer. Dispatch
    /// errors are logged via `debug!()` and never surface in the tool result.
    ///
    /// Returns `true` if the position is a symbol (or no capable server
    /// exists), `false` if not (e.g. a keyword).
    async fn prepare_rename_check(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_rename,
                Some(DispatchMethod::Rename),
            )
            .await;

        for client_mutex in &servers {
            if cancel.is_cancelled() {
                break;
            }

            let Ok(uri) = self
                .client_manager
                .open_document_on(path, client_mutex, parent_id.map(str::to_string))
                .await
            else {
                continue;
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id.map(str::to_string));
            client.set_cancel_token(cancel.clone());
            let response = client.prepare_rename(&uri, line_0, col).await;
            client.close_tracked_document(&uri).await;
            drop(client);

            match response {
                Ok(v) if v.is_null() => return false, // null → not a symbol
                Ok(_) => return true,                 // range → symbol
                Err(e) => {
                    debug!(
                        source = Source::LspDispatch.as_str(),
                        "prepare_rename failed: {e}"
                    );
                }
            }
        }

        // No capable server, all errored, or cancelled — assume symbol
        true
    }

    /// Enriches a symbol at a position with LSP data.
    ///
    /// Sends all enrichment queries (references, call hierarchy, implementations,
    /// type hierarchy) for every symbol. The server decides what returns results.
    ///
    /// Callers gate enrichment before calling this method — `GrepServer::run`
    /// only enriches `Symbol`/`PrepareRenameSymbol` hits, so a hit
    /// `prepare_rename_check` did not confirm as a symbol is rendered as a
    /// plain reference atom and never reaches this method (it is not dropped).
    ///
    /// Opens the document once on the union of all capability-filtered servers,
    /// runs all four enrichment methods (skipping their per-method open/close),
    /// then closes the document on each server. This avoids `didClose` between
    /// methods causing the server to evict document state.
    #[allow(
        clippy::too_many_lines,
        reason = "orchestration across four LSP methods"
    )]
    async fn enrich_at_position(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<SymbolEnrichment> {
        // Check the enrichment cache for workspace-rooted files.
        let resolved_root = self.fs_manager.resolve_root(path);
        let key = EnrichmentKey {
            file: path.to_path_buf(),
            line: line_0,
            col,
        };
        if resolved_root.is_some()
            && let Some(ref idx_arc) = self.symbol_index
            && let Ok(mut idx) = idx_arc.lock()
            && let Some(cached) = idx.get_enrichment(&key, &self.fs_manager)
        {
            return Some(cached);
        }

        // Collect the union of servers across all enrichment capabilities.
        let ref_servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_references,
                Some(DispatchMethod::References),
            )
            .await;
        let call_servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_call_hierarchy,
                Some(DispatchMethod::CallHierarchy),
            )
            .await;
        let impl_servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_implementation,
                Some(DispatchMethod::Implementation),
            )
            .await;
        let type_servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_type_hierarchy,
                Some(DispatchMethod::TypeHierarchy),
            )
            .await;

        let all_servers = {
            let mut seen = HashSet::new();
            ref_servers
                .iter()
                .chain(call_servers.iter())
                .chain(impl_servers.iter())
                .chain(type_servers.iter())
                .filter(|s| seen.insert(Arc::as_ptr(s)))
                .cloned()
                .collect::<Vec<_>>()
        };

        // Open the document once on each server.
        let mut uri_opt: Option<String> = None;
        let mut opened_servers = Vec::new();
        for server in &all_servers {
            match self
                .client_manager
                .open_document_on(path, server, parent_id.map(str::to_string))
                .await
            {
                Ok(u) => {
                    uri_opt = Some(u);
                    opened_servers.push(Arc::clone(server));
                }
                Err(e) => {
                    debug!(
                        source = Source::LspDispatch.as_str(),
                        "enrichment open failed: {e}"
                    );
                }
            }
        }

        let pre_uri = uri_opt.as_deref();

        // Run all enrichment methods with the document already open.
        // Check cancellation between each method so we don't burn
        // through fetch attempts after the token has already fired.
        let ref_lines = self
            .fetch_references(path, line_0, col, parent_id, pre_uri, cancel)
            .await;

        let (incoming_calls, outgoing_calls) = if cancel.is_cancelled() {
            (Vec::new(), Vec::new())
        } else {
            self.fetch_call_hierarchy(path, line_0, col, parent_id, pre_uri, cancel)
                .await
        };

        let implementations = if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.fetch_implementations(path, line_0, col, parent_id, pre_uri, cancel)
                .await
        };

        let (supertypes, subtypes) = if cancel.is_cancelled() {
            (Vec::new(), Vec::new())
        } else {
            self.fetch_type_hierarchy(path, line_0, col, parent_id, pre_uri, cancel)
                .await
        };

        // Close the document once on each server.
        if let Some(ref uri) = uri_opt {
            for server in &opened_servers {
                server.lock().await.close_tracked_document(uri).await;
            }
        }

        // If cancelled, return None so the caller's is_cancelled()
        // check triggers immediately.
        if cancel.is_cancelled() {
            return None;
        }

        let enrichment = SymbolEnrichment {
            ref_lines,
            incoming_calls,
            outgoing_calls,
            implementations,
            supertypes,
            subtypes,
        };

        // Store in the enrichment cache for workspace-rooted files.
        if let Some(root) = resolved_root
            && let Some(ref idx_arc) = self.symbol_index
            && let Ok(mut idx) = idx_arc.lock()
        {
            let generation = self.fs_manager.root_generation(&root);
            let source_mtime = std::fs::metadata(path).ok().map(|m| mtime_nanos(&m));
            idx.cache_enrichment(
                key,
                Witness {
                    root,
                    generation,
                    source_mtime,
                },
                enrichment.clone(),
            );
        }

        Some(enrichment)
    }

    /// Fetches references via priority chain dispatch.
    ///
    /// When `pre_opened_uri` is `Some`, the document is already open on
    /// all servers — skips `open_document_on` / `close_document`. When
    /// `None`, each server attempt opens and closes independently.
    async fn fetch_references(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<&str>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> HashSet<AtomId> {
        let servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_references,
                Some(DispatchMethod::References),
            )
            .await;

        for client_mutex in &servers {
            if cancel.is_cancelled() {
                break;
            }

            let owned_uri;
            let uri: &str = if let Some(u) = pre_opened_uri {
                u
            } else {
                let Ok(u) = self
                    .client_manager
                    .open_document_on(path, client_mutex, parent_id.map(str::to_string))
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id.map(str::to_string));
            client.set_cancel_token(cancel.clone());
            let result = client.references(uri, line_0, col, true).await;
            if pre_opened_uri.is_none() {
                client.close_tracked_document(uri).await;
            }
            drop(client);

            match result {
                Ok(Value::Array(refs)) => {
                    let mut ref_lines: HashSet<AtomId> = HashSet::new();
                    for r in &refs {
                        if let Some(file) = extract_location_path(r)
                            && let Some(line) = extract_start_line(r)
                        {
                            ref_lines.insert(AtomId::new(&file, line));
                        }
                    }
                    return ref_lines;
                }
                Ok(_) => {}
                Err(e) => {
                    debug!(
                        source = Source::LspDispatch.as_str(),
                        "references failed: {e}"
                    );
                }
            }
        }

        HashSet::new()
    }

    /// Fetches incoming and outgoing calls via priority chain dispatch.
    ///
    /// When `pre_opened_uri` is `Some`, the document is already open —
    /// skips open/close. Otherwise opens, runs the full prepare →
    /// incoming → outgoing sequence on a single server, then closes.
    async fn fetch_call_hierarchy(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<&str>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> (Vec<Edge>, Vec<Edge>) {
        let servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_call_hierarchy,
                Some(DispatchMethod::CallHierarchy),
            )
            .await;

        for client_mutex in &servers {
            if cancel.is_cancelled() {
                break;
            }

            let owned_uri;
            let uri: &str = if let Some(u) = pre_opened_uri {
                u
            } else {
                let Ok(u) = self
                    .client_manager
                    .open_document_on(path, client_mutex, parent_id.map(str::to_string))
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id.map(str::to_string));
            client.set_cancel_token(cancel.clone());
            let prepare = client.prepare_call_hierarchy(uri, line_0, col).await;
            let result = match prepare {
                Ok(Value::Array(ref items)) if !items.is_empty() => {
                    let item = &items[0];
                    let incoming = match client.incoming_calls(item).await {
                        Ok(Value::Array(calls)) => calls
                            .iter()
                            .filter_map(|c| extract_edge(c.get("from")?))
                            .collect(),
                        _ => Vec::new(),
                    };
                    let outgoing = match client.outgoing_calls(item).await {
                        Ok(Value::Array(calls)) => calls
                            .iter()
                            .filter_map(|c| extract_edge(c.get("to")?))
                            .collect(),
                        _ => Vec::new(),
                    };
                    Some((incoming, outgoing))
                }
                Ok(_) => None,
                Err(e) => {
                    debug!(
                        source = Source::LspDispatch.as_str(),
                        "prepare_call_hierarchy failed: {e}"
                    );
                    None
                }
            };
            if pre_opened_uri.is_none() {
                client.close_tracked_document(uri).await;
            }
            drop(client);

            if let Some(calls) = result {
                return calls;
            }
        }

        (Vec::new(), Vec::new())
    }

    /// Fetches implementation locations via priority chain dispatch.
    ///
    /// When `pre_opened_uri` is `Some`, the document is already open —
    /// skips open/close.
    async fn fetch_implementations(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<&str>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Vec<AtomId> {
        let servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_implementation,
                Some(DispatchMethod::Implementation),
            )
            .await;

        for client_mutex in &servers {
            if cancel.is_cancelled() {
                break;
            }

            let owned_uri;
            let uri: &str = if let Some(u) = pre_opened_uri {
                u
            } else {
                let Ok(u) = self
                    .client_manager
                    .open_document_on(path, client_mutex, parent_id.map(str::to_string))
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id.map(str::to_string));
            client.set_cancel_token(cancel.clone());
            let result = client.implementation(uri, line_0, col).await;
            if pre_opened_uri.is_none() {
                client.close_tracked_document(uri).await;
            }
            drop(client);

            match result {
                Ok(Value::Array(locs)) => {
                    return locs
                        .iter()
                        .filter_map(|loc| {
                            let file = extract_location_path(loc)?;
                            let line = extract_start_line(loc)?;
                            Some(AtomId::new(&file, line))
                        })
                        .collect();
                }
                Ok(_) => {}
                Err(e) => {
                    debug!(
                        source = Source::LspDispatch.as_str(),
                        "implementation failed: {e}"
                    );
                }
            }
        }

        Vec::new()
    }

    /// Fetches supertypes and subtypes via priority chain dispatch.
    ///
    /// When `pre_opened_uri` is `Some`, the document is already open —
    /// skips open/close. Otherwise opens, runs the full prepare →
    /// supertypes → subtypes sequence on a single server, then closes.
    async fn fetch_type_hierarchy(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<&str>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> (Vec<Edge>, Vec<Edge>) {
        let servers = self
            .client_manager
            .get_servers(
                path,
                LspServer::supports_type_hierarchy,
                Some(DispatchMethod::TypeHierarchy),
            )
            .await;

        for client_mutex in &servers {
            if cancel.is_cancelled() {
                break;
            }

            let owned_uri;
            let uri: &str = if let Some(u) = pre_opened_uri {
                u
            } else {
                let Ok(u) = self
                    .client_manager
                    .open_document_on(path, client_mutex, parent_id.map(str::to_string))
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id.map(str::to_string));
            client.set_cancel_token(cancel.clone());
            let prepare = client.prepare_type_hierarchy(uri, line_0, col).await;
            let result = match prepare {
                Ok(Value::Array(ref items)) if !items.is_empty() => {
                    let item = &items[0];
                    let supertypes = match client.supertypes(item).await {
                        Ok(Value::Array(types)) => types.iter().filter_map(extract_edge).collect(),
                        _ => Vec::new(),
                    };
                    let subtypes = match client.subtypes(item).await {
                        Ok(Value::Array(types)) => types.iter().filter_map(extract_edge).collect(),
                        _ => Vec::new(),
                    };
                    Some((supertypes, subtypes))
                }
                Ok(_) => None,
                Err(e) => {
                    debug!(
                        source = Source::LspDispatch.as_str(),
                        "prepare_type_hierarchy failed: {e}"
                    );
                    None
                }
            };
            if pre_opened_uri.is_none() {
                client.close_tracked_document(uri).await;
            }
            drop(client);

            if let Some(types) = result {
                return types;
            }
        }

        (Vec::new(), Vec::new())
    }

    /// Searches workspace roots for pattern matches using the `grep-*` crates
    /// (ripgrep's internals). Walks files in parallel and returns matched
    /// strings and per-file line numbers in a single pass per file.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid regex.
    fn ripgrep_matches(
        pattern: &str,
        roots: &[PathBuf],
        exclude: Option<&Arc<ResolvedGlob>>,
        include_gitignored: bool,
        include_hidden: bool,
        fs_manager: &Arc<FilesystemManager>,
    ) -> Result<RipgrepMatches> {
        use ignore::WalkState;
        use std::sync::Mutex as StdMutex;

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(true)
            .build(pattern)
            .map_err(|e| anyhow!("Invalid regex pattern: {e}"))?;

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
            let walker = WalkBuilder::new(root)
                .git_ignore(skip_gitignored && !root_is_file)
                .hidden(skip_hidden && !root_is_file)
                .build_parallel();

            walker.run(|| {
                let matcher = matcher.clone();
                let exclude = exclude.cloned();
                let root = root.clone();
                let fs_manager = Arc::clone(fs_manager);
                let mut state = CollectOnDrop {
                    local: ThreadMatches::default(),
                    collected: Arc::clone(&collected),
                };

                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    let path = entry.path();
                    // Record traversed directories as result-cache membership
                    // witnesses (a new file added here bumps this dir's mtime).
                    if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                        state.local.dirs.push(path.to_path_buf());
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

                    if let Some(rg) = &exclude
                        && rg.is_match(path, &root)
                    {
                        return WalkState::Continue;
                    }

                    // Skip binary files — no meaningful text matches
                    if let Some(md) = &metadata
                        && fs_manager.is_binary(path, md)
                    {
                        return WalkState::Continue;
                    }

                    let path_str = path.to_string_lossy().to_string();
                    let mut sink = MatchSink {
                        matcher: &matcher,
                        path: &path_str,
                        local: &mut state.local,
                    };

                    if let Err(e) = Searcher::new().search_path(&matcher, path, &mut sink) {
                        debug!("grep: skipping {path_str}: {e}");
                    }

                    WalkState::Continue
                })
            });
        }

        let parts = harvest(collected)?;

        Ok(RipgrepMatches::merge(parts))
    }
}

// ─── Rendering ─────────────────────────────────────────────────────────

/// Renders grep results with page-based paging.
///
/// Every result is one atom — `path:line  <source line>` (decision 024).
/// The top-level list is the matches (definitions and occurrences alike),
/// each a full atom; a definition additionally carries nested, labeled edge
/// groups (`calls:`/`called by:`/impls/super-/subtypes/refs). Grouped by
/// workspace root (bare absolute path header) for absolute patterns, or under
/// a `cwd: ~/…` context header for cwd-scoped searches.
///
/// Returns the full unpaginated output. Pagination is applied by the
/// caller (`execute`).
fn render_results(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
    fs_manager: &FilesystemManager,
    cwd: Option<&Path>,
) -> String {
    use std::fmt::Write;

    let mut full = String::new();
    // The membership set: every top-level match atom's (file, line). An edge
    // whose target is in this set collapses to a citation; one outside is read
    // full. Collected across the WHOLE result so a citation in one root section
    // can point at an atom rendered full in another (the paging invariant).
    let top_level = collect_top_level_atoms(enrichments);
    let mut sources = SourceLines::new();

    if let Some(cwd) = cwd {
        // cwd-scoped: one section, `cwd: ~/path` header, cwd-relative paths.
        let compressed = super::compress_home(cwd);
        if fs_manager.resolve_root(cwd).is_some() {
            let _ = writeln!(full, "cwd: {compressed}");
        } else {
            let _ = writeln!(full, "cwd: {compressed} {NO_LSP_LABEL}");
        }
        let all_indices: Vec<usize> = (0..enrichments.len()).collect();
        render_section(
            enrichments,
            &all_indices,
            fs_manager,
            &top_level,
            &mut sources,
            &mut full,
            Some(cwd),
        );
    } else {
        // Absolute glob: group by workspace root.
        let mut root_items: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        let mut oor_items: Vec<usize> = Vec::new();
        for (i, (hit, _)) in enrichments.iter().enumerate() {
            match fs_manager.resolve_root(&hit.file) {
                Some(root) => root_items.entry(root).or_default().push(i),
                None => oor_items.push(i),
            }
        }

        // LSP warning when all results are outside workspace roots.
        if root_items.is_empty() && !oor_items.is_empty() {
            let _ = writeln!(full, "{NO_LSP_LABEL}");
        }

        for (root, indices) in &root_items {
            if !full.is_empty() {
                full.push('\n');
            }
            let _ = writeln!(full, "{}", root.display());
            render_section(
                enrichments,
                indices,
                fs_manager,
                &top_level,
                &mut sources,
                &mut full,
                None,
            );
        }
        // Out-of-root hits grouped under their parent directory path.
        let mut oor_by_parent: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for &i in &oor_items {
            let (hit, _) = &enrichments[i];
            let parent = hit
                .file
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf();
            oor_by_parent.entry(parent).or_default().push(i);
        }
        for (parent, indices) in &oor_by_parent {
            if !full.is_empty() {
                full.push('\n');
            }
            let _ = writeln!(full, "{}", parent.display());
            render_section(
                enrichments,
                indices,
                fs_manager,
                &top_level,
                &mut sources,
                &mut full,
                None,
            );
        }
    }

    let trimmed_len = full.trim_end().len();
    full.truncate(trimmed_len);
    full
}

/// The [`AtomId`] of a hit — the definition line for a `Symbol`, the hit line
/// otherwise.
fn hit_atom(hit: &GrepHit) -> AtomId {
    let line = match &hit.classification {
        HitClass::Symbol { symbol } => symbol.line,
        _ => hit.line,
    };
    AtomId::new(hit.file.to_string_lossy().as_ref(), line)
}

/// Collects the set of top-level match atoms across the whole result — the
/// membership test that decides edge collapse.
///
/// Top level is always full and is the canonical home of every atom; an edge
/// pointing here collapses to a `path:line  name` citation, an edge pointing
/// elsewhere is read full. The set spans every root section so a citation is
/// an absolute pointer to a guaranteed-present atom regardless of which page
/// holds the full form (decision 024, the paging invariant).
fn collect_top_level_atoms(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
) -> HashSet<AtomId> {
    enrichments.iter().map(|(hit, _)| hit_atom(hit)).collect()
}

/// Renders one root section as one-atom output (decision 024).
///
/// Every match is a top-level atom `path:line  <source line>`, ordered by
/// `(file, line)` for byte-stable output (the misc-32 determinism pattern). A
/// definition (a `Symbol`/`PrepareRenameSymbol` hit) additionally carries
/// nested, labeled edge groups (`calls:`/`impls:`/`supertypes:`/`subtypes:`/
/// `refs:`); each edge is itself an atom. An edge whose target is a top-level
/// match collapses to a `path:line  name` citation (the full line is already
/// present); an edge whose target is not a match is read full — the navigation
/// Catenary does for the agent. A definition with no edges renders as its lean
/// single atom (fish-eye).
fn render_section(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
    indices: &[usize],
    fs_manager: &FilesystemManager,
    top_level: &HashSet<AtomId>,
    sources: &mut SourceLines,
    output: &mut String,
    cwd: Option<&Path>,
) {
    use std::fmt::Write;

    // Path display: cwd-relative when cwd is set, root-relative otherwise.
    let rel = |file: &str| -> String {
        cwd.map_or_else(
            || display_path(file, fs_manager),
            |base| {
                Path::new(file)
                    .strip_prefix(base)
                    .map_or_else(|_| file.to_string(), |r| r.to_string_lossy().to_string())
            },
        )
    };

    // Citation names: a collapsed edge shows `path:line  name`. A symbol atom
    // cites by its lean name (decision 024); but a citation target can also be a
    // plain `Reference` occurrence (a grep hit that is itself referenced by a
    // definition), which has no name. `atom_texts` carries every top-level atom's
    // full source line so such a citation falls back to it instead of rendering
    // blank (bug 48).
    let atom_names = collect_atom_names(enrichments);
    let atom_texts = collect_atom_texts(enrichments);
    let citations = AtomCitations {
        top_level,
        names: &atom_names,
        texts: &atom_texts,
    };

    // Order the section's hits by atom id (`(file, line)`) — reproducible bytes.
    let mut ordered: BTreeMap<AtomId, usize> = BTreeMap::new();
    for &i in indices {
        ordered.insert(hit_atom(enrichments[i].0), i);
    }

    for &i in ordered.values() {
        let (hit, enrichment) = &enrichments[i];

        // Blank line between top-level atoms (and their edge blocks).
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }

        // Top-level atom: ALWAYS the full source line, verbatim.
        let rel_path = rel(&hit.file.to_string_lossy());
        let line_1 = hit_atom(hit).line + 1;
        let _ = writeln!(output, "{rel_path}:{line_1}  {}", hit.matched_text);

        // Only definitions carry edge groups; occurrences are lean atoms.
        if !matches!(
            hit.classification,
            HitClass::Symbol { .. } | HitClass::PrepareRenameSymbol
        ) {
            continue;
        }
        let Some(enrichment) = enrichment else {
            continue;
        };

        render_edge_groups(output, enrichment, &citations, sources, &rel);
    }
}

/// Builds the citation-name lookup: each top-level definition atom's [`AtomId`]
/// → its symbol name. A collapsed edge cites by this name.
fn collect_atom_names(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
) -> HashMap<AtomId, String> {
    let mut names = HashMap::new();
    for (hit, _) in enrichments {
        let name = match &hit.classification {
            HitClass::Symbol { symbol } => symbol.name.clone(),
            HitClass::PrepareRenameSymbol => hit.matched_text.clone(),
            HitClass::Reference => continue,
        };
        names.insert(hit_atom(hit), name);
    }
    names
}

/// Builds the citation-text lookup: each top-level atom's [`AtomId`] → its full
/// source line. Unlike [`collect_atom_names`] this includes `Reference`
/// occurrences, so a citation whose target has no symbol name (a plain
/// occurrence referenced by a definition) can fall back to the verbatim line
/// instead of rendering blank (bug 48).
fn collect_atom_texts(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
) -> HashMap<AtomId, String> {
    enrichments
        .iter()
        .map(|(hit, _)| (hit_atom(hit), hit.matched_text.clone()))
        .collect()
}

/// The citation lookups shared across edge rendering: the membership set that
/// decides edge collapse, plus the name and full-text maps a collapsed citation
/// draws on. Bundled so the edge renderers stay within argument limits.
struct AtomCitations<'a> {
    /// Every top-level match atom — an edge whose target is here collapses to a
    /// citation.
    top_level: &'a HashSet<AtomId>,
    /// Top-level definition atoms' names, for lean citations (decision 024).
    names: &'a HashMap<AtomId, String>,
    /// Every top-level atom's full source line — the fallback when a cited atom
    /// has no name (a plain `Reference` occurrence), so a citation is never
    /// blank (bug 48).
    texts: &'a HashMap<AtomId, String>,
}

/// Renders the labeled edge groups under a definition atom (decision 024).
///
/// Each edge is an atom: collapsed to `path:line  name` when its target is a
/// top-level match (the full line is present elsewhere in the result), read
/// full (`path:line  <source line>`) otherwise. Edges are deduplicated by
/// atom `(file, line)`, so a back-edge of a cycle collapses against its peer.
fn render_edge_groups(
    output: &mut String,
    enrichment: &SymbolEnrichment,
    citations: &AtomCitations,
    sources: &mut SourceLines,
    rel: &impl Fn(&str) -> String,
) {
    use std::fmt::Write;

    // Dedup edges across all this definition's groups by atom id.
    let mut seen: HashSet<AtomId> = HashSet::new();

    // calls: — outgoing call edges, ordered by atom id.
    let mut calls: BTreeMap<AtomId, &Edge> = BTreeMap::new();
    for c in &enrichment.outgoing_calls {
        calls.entry(c.target.clone()).or_insert(c);
    }
    if !calls.is_empty() {
        let _ = writeln!(output, "\tcalls:");
        for (id, c) in &calls {
            if seen.insert(id.clone()) {
                render_edge_atom(output, &c.name, id, citations, sources, rel);
            }
        }
    }

    // impls: — implementation locations, ordered by atom id. LSP gives only
    // `(file, line)`; a collapsed citation borrows the matched atom's name.
    let mut impls: BTreeSet<AtomId> = BTreeSet::new();
    for id in &enrichment.implementations {
        impls.insert(id.clone());
    }
    let impls: Vec<AtomId> = impls
        .into_iter()
        .filter(|atom| seen.insert(atom.clone()))
        .collect();
    if !impls.is_empty() {
        let _ = writeln!(output, "\timpls:");
        for id in &impls {
            let name = citations.names.get(id).map_or("", String::as_str);
            render_edge_atom(output, name, id, citations, sources, rel);
        }
    }

    // supertypes: / subtypes: — type hierarchy edges, ordered by (file, line).
    render_type_group(
        output,
        "supertypes",
        &enrichment.supertypes,
        &mut seen,
        citations,
        sources,
        rel,
    );
    render_type_group(
        output,
        "subtypes",
        &enrichment.subtypes,
        &mut seen,
        citations,
        sources,
        rel,
    );

    // refs: — textDocument/references plus incoming call edges, ordered by atom
    // id, deduplicated against the edges above.
    let mut refs: BTreeMap<AtomId, String> = BTreeMap::new();
    for id in &enrichment.ref_lines {
        refs.entry(id.clone()).or_default();
    }
    for caller in &enrichment.incoming_calls {
        refs.entry(caller.target.clone())
            .or_insert_with(|| caller.name.clone());
    }
    let refs: Vec<(AtomId, String)> = refs
        .into_iter()
        .filter(|(atom, _)| seen.insert(atom.clone()))
        .collect();
    if !refs.is_empty() {
        let _ = writeln!(output, "\trefs:");
        for (id, caller_name) in &refs {
            // Prefer the cited atom's own name; fall back to an incoming
            // caller's name when the reference is not itself a match.
            let name = citations
                .names
                .get(id)
                .map_or(caller_name.as_str(), String::as_str);
            render_edge_atom(output, name, id, citations, sources, rel);
        }
    }
}

/// Renders a type-hierarchy edge group (`supertypes:`/`subtypes:`).
fn render_type_group(
    output: &mut String,
    label: &str,
    edges: &[Edge],
    seen: &mut HashSet<AtomId>,
    citations: &AtomCitations,
    sources: &mut SourceLines,
    rel: &impl Fn(&str) -> String,
) {
    use std::fmt::Write;

    let mut by_atom: BTreeMap<AtomId, &Edge> = BTreeMap::new();
    for t in edges {
        by_atom.entry(t.target.clone()).or_insert(t);
    }
    let by_atom: Vec<(AtomId, &Edge)> = by_atom
        .into_iter()
        .filter(|(atom, _)| seen.insert(atom.clone()))
        .collect();
    if by_atom.is_empty() {
        return;
    }
    let _ = writeln!(output, "\t{label}:");
    for (id, t) in &by_atom {
        render_edge_atom(output, &t.name, id, citations, sources, rel);
    }
}

/// Renders a single edge as an atom (decision 024).
///
/// Collapsed to `path:line  name` when the target is a top-level match (the
/// paging-invariant citation — an absolute pointer to an atom rendered full
/// elsewhere in the same result), read full `path:line  <source line>`
/// otherwise. When the source line is unavailable and the target is not a
/// match, the edge's own `name` is the fallback so the atom is never empty.
///
/// A citation whose target has no name — a plain `Reference` occurrence
/// referenced by a definition — falls back to that target's full source line
/// from `citations.texts`, never rendering a blank raw field (bug 48).
fn render_edge_atom(
    output: &mut String,
    name: &str,
    atom: &AtomId,
    citations: &AtomCitations,
    sources: &mut SourceLines,
    rel: &impl Fn(&str) -> String,
) {
    use std::fmt::Write;

    let line_1 = atom.line + 1;
    let rel_path = rel(&atom.file);
    if citations.top_level.contains(atom) {
        // Citation: the full line is a top-level atom elsewhere in the result.
        // Prefer the lean name; when the cited atom has none (a plain occurrence)
        // fall back to its verbatim source line so the citation is never blank.
        let label = if name.is_empty() {
            citations.texts.get(atom).map_or(name, String::as_str)
        } else {
            name
        };
        let _ = writeln!(output, "\t\t{rel_path}:{line_1}  {label}");
    } else {
        // Read the line in for the agent (bounded to new targets).
        let text = sources
            .line(Path::new(&atom.file), atom.line)
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .unwrap_or(name);
        let _ = writeln!(output, "\t\t{rel_path}:{line_1}  {text}");
    }
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
        // Flush when this thread saw any matches OR any files: the changed-set
        // baseline (WS31) needs every visited file, even from a thread whose
        // files held no pattern match.
        if local.file_lines.is_empty() && local.files.is_empty() {
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
        // (enrichment gating) and the enrichment query at the symbol.
        let Some(first) = self.matcher.find(line_bytes).ok().flatten() else {
            return Ok(true);
        };
        let col = u32::try_from(first.start()).unwrap_or(0);
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

        Ok(true)
    }
}

// ─── Alternation splitting ────────────────────────────────────────────

/// Result of a ripgrep line search.
#[derive(Default)]
struct RipgrepMatches {
    /// Per-file line numbers.
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line `(full source line, first-match column)` — the atom
    /// text (one-atom model, decision 024) plus the first match's column for
    /// hit classification and `prepareRename` positioning.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
    /// Directories the walk traversed. Their mtimes are the result cache's
    /// membership witnesses: a new matching file added anywhere under the scope
    /// bumps its parent directory's mtime (which the walk visited), so a stale
    /// cached page is invalidated even though no existing match's mtime moved
    /// (bug #26 add/remove gap).
    dirs: Vec<PathBuf>,
    /// Every regular file the walk visited, with its `(absolute path, mtime)`
    /// — not just the files that matched the pattern. Feeds the WS31 changed-set
    /// baseline diff (Consumer A): the manager filters these to the union of
    /// registered watch globs, diffs against the per-root baseline, and routes
    /// the delta per server. The stat is free here — the walk already reads each
    /// file (`grep_server.rs` ripgrep walk).
    files: Vec<(PathBuf, i64)>,
}

impl RipgrepMatches {
    /// Merges per-thread match accumulators into a single result.
    fn merge(parts: Vec<ThreadMatches>) -> Self {
        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>> = HashMap::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut files: Vec<(PathBuf, i64)> = Vec::new();

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
            dirs.extend(part.dirs);
            files.extend(part.files);
        }

        Self {
            file_lines,
            file_line_texts,
            dirs,
            files,
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
    /// Directories visited by this thread (result-cache membership witnesses).
    dirs: Vec<PathBuf>,
    /// Every regular file this thread visited, `(absolute path, mtime)` — the
    /// WS31 changed-set baseline observation set.
    files: Vec<(PathBuf, i64)>,
}

/// Splits a regex pattern on top-level `|` alternation.
///
/// Only splits on `|` when depth == 0 and not inside a character class.
/// `foo|bar` → `["foo", "bar"]`. `(foo|bar)_baz` → `["(foo|bar)_baz"]`.
fn split_alternation(pattern: &str) -> Vec<String> {
    let mut arms = Vec::new();
    let mut depth: usize = 0;
    let mut in_class = false;
    let mut start = 0;
    let mut escaped = false;

    for (i, ch) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if in_class {
            if ch == ']' {
                in_class = false;
            }
            continue;
        }
        match ch {
            '[' => in_class = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                arms.push(pattern[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    arms.push(pattern[start..].to_string());
    arms.retain(|a| !a.is_empty());
    if arms.is_empty() {
        arms.push(pattern.to_string());
    }
    arms
}

// ─── LSP JSON extraction helpers ────────────────────────────────────────

/// Extracts a file path from an LSP Location's `uri` field.
///
/// Strips the `file://` prefix from a `file://` URI. Returns `None`
/// for non-file URIs or missing fields.
fn extract_location_path(location: &Value) -> Option<String> {
    location
        .get("uri")?
        .as_str()?
        .strip_prefix("file://")
        .map(str::to_string)
}

/// Extracts the start line (0-based) from an LSP Location's range.
fn extract_start_line(location: &Value) -> Option<u32> {
    u32::try_from(location.get("range")?.get("start")?.get("line")?.as_u64()?).ok()
}

/// Extracts an [`Edge`] from a `CallHierarchyItem` or `TypeHierarchyItem` JSON
/// value — both share the `name`/`uri`/`range` shape, so one extractor serves
/// both.
///
/// The one-atom model carries only the edge's name and target `(file, line)`;
/// the LSP kind / container / deprecation tags are not rendered, so they are
/// not read.
fn extract_edge(item: &Value) -> Option<Edge> {
    let name = item.get("name")?.as_str()?.to_string();
    let file = item
        .get("uri")?
        .as_str()?
        .strip_prefix("file://")
        .map(str::to_string)?;
    let line = u32::try_from(item.get("range")?.get("start")?.get("line")?.as_u64()?).ok()?;
    Some(Edge {
        name,
        target: AtomId::new(&file, line),
    })
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

    // ─── split_alternation tests ─────────────────────────────────────────

    #[test]
    fn test_split_top_level() {
        assert_eq!(split_alternation("foo|bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn test_split_nested_no_split() {
        assert_eq!(split_alternation("(foo|bar)_baz"), vec!["(foo|bar)_baz"]);
    }

    #[test]
    fn test_split_character_class() {
        assert_eq!(split_alternation("[a|b]_c"), vec!["[a|b]_c"]);
    }

    #[test]
    fn test_split_three_arms() {
        assert_eq!(split_alternation("a|b|c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_no_alternation() {
        assert_eq!(split_alternation("foobar"), vec!["foobar"]);
    }

    #[test]
    fn test_split_escaped_pipe() {
        assert_eq!(split_alternation(r"foo\|bar"), vec![r"foo\|bar"]);
    }

    // ─── One-atom rendering helpers ─────────────────────────────────────

    /// Build a `Symbol`-classified `GrepHit`. `src` is the verbatim source
    /// line — the atom the renderer prints (`--only-matching` is dropped).
    fn sym_hit(file: &str, line: u32, name: &str, kind: &str, src: &str) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            col: 0,
            matched_text: src.to_string(),
            classification: HitClass::Symbol {
                symbol: Symbol {
                    name: name.to_string(),
                    kind: kind.to_string(),
                    line,
                    end_line: line + 10,
                    scope: None,
                    scope_kind: None,
                    deprecated: false,
                },
            },
        }
    }

    /// Build a `Reference`-classified `GrepHit` carrying its full source line.
    fn ref_hit(file: &str, line: u32, src: &str) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            col: 0,
            matched_text: src.to_string(),
            classification: HitClass::Reference,
        }
    }

    fn test_fs(root: &str) -> FilesystemManager {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from(root)]);
        fs
    }

    fn empty_enrichment() -> SymbolEnrichment {
        SymbolEnrichment {
            ref_lines: HashSet::new(),
            incoming_calls: Vec::new(),
            outgoing_calls: Vec::new(),
            implementations: Vec::new(),
            supertypes: Vec::new(),
            subtypes: Vec::new(),
        }
    }

    // ─── Rendering ──────────────────────────────────────────────────────

    /// Helper: render + paginate with no enrichment.
    fn render(hits: &[GrepHit], budget: usize, page: usize, fs: &FilesystemManager) -> String {
        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> =
            hits.iter().map(|h| (h, None)).collect();
        let full = render_results(&enrichments, fs, None);
        paginate(&full, budget, page)
    }

    #[test]
    fn atom_is_the_full_source_line_no_kind() {
        let fs = test_fs("/project");
        // A mid-line comment match: the atom is the FULL line, not the token,
        // and carries no `<Kind>` label.
        let hits = [ref_hit(
            "/project/src/a.rs",
            9,
            "let x = 1; // configure the widget",
        )];

        let output = render(&hits, 10_000, 1, &fs);

        assert!(
            output.contains("src/a.rs:10  let x = 1; // configure the widget"),
            "full line atom, 1-based: {output}"
        );
        assert!(!output.contains('<'), "no kind label anywhere: {output}");
    }

    #[test]
    fn definitions_and_references_are_both_top_level_atoms() {
        let fs = test_fs("/project");
        // A definition-like hit and a plain occurrence — both top-level atoms.
        let hits = [
            sym_hit(
                "/project/data/config.yaml",
                15,
                "handle",
                "function",
                "handle: process",
            ),
            ref_hit("/project/src/util.rs", 30, "    handle(input);"),
        ];

        let output = render(&hits, 10_000, 1, &fs);

        assert!(
            output.contains("config.yaml:16  handle: process"),
            "definition atom present: {output}"
        );
        assert!(
            output.contains("util.rs:31      handle(input);")
                || output.contains("util.rs:31  ") && output.contains("handle(input);"),
            "occurrence atom not dropped: {output}"
        );
        assert!(!output.contains('<'), "no kind label: {output}");
    }

    #[test]
    fn plain_references_render_full_lines() {
        let fs = test_fs("/project");
        let hits = [
            ref_hit("/project/data/notes.txt", 5, "see the pattern below"),
            ref_hit("/project/src/main.rs", 100, "    let pattern = compile();"),
        ];

        let output = render(&hits, 10_000, 1, &fs);

        // Both atoms rendered with 1-based lines and their full source text.
        assert!(
            output.contains("notes.txt:6  see the pattern below"),
            "bare ref atom: {output}"
        );
        assert!(
            output.contains("main.rs:101  ") && output.contains("let pattern = compile();"),
            "second ref atom: {output}"
        );
    }

    #[test]
    fn citation_to_reference_atom_is_not_blank() {
        // Repro for bug 48 empty-raw `refs` lines.
        //
        // When an edge (here a `refs:` entry) points at a (file, line) that is
        // ALSO a top-level match atom, the renderer collapses it to a citation
        // `path:line  name` instead of re-reading the source. But citation names
        // come from `collect_atom_names`, which only records
        // Symbol/PrepareRenameSymbol atoms — a plain Reference occurrence has no
        // name. The result is a citation with an EMPTY raw field:
        // `\t\tpath:line  ` with nothing after it. Decision 024 / bug 31: an
        // atom must never render blank — the renderer already HAS the cited
        // atom's full source line (it is a top-level match), so a citation can
        // and must surface it.
        let fs = test_fs("/project");

        // The enriched definition.
        let def = sym_hit(
            "/project/src/feeder.rs",
            78,
            "DiagnosticFeeder",
            "trait",
            "pub trait DiagnosticFeeder {",
        );
        // A plain textual occurrence that is ALSO a reference target of `def`.
        let user = ref_hit(
            "/project/src/user.rs",
            11,
            "use crate::bridge::linter::DiagnosticFeeder;",
        );

        // Enrichment: references include the occurrence's atom (0-based line).
        let enrichment = SymbolEnrichment {
            ref_lines: HashSet::from([AtomId::new("/project/src/user.rs", 11)]),
            ..empty_enrichment()
        };

        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> =
            vec![(&def, Some(enrichment)), (&user, None)];
        let output = render_results(&enrichments, &fs, None);

        // The `refs:` citation is the DOUBLE-TAB-indented line under the def
        // (`\t\tsrc/user.rs:12  …`) — distinct from the top-level atom for the
        // same occurrence (rendered with no indent). Currently it renders blank
        // (`\t\tsrc/user.rs:12  ` with nothing after) because a Reference atom
        // has no citation name. The renderer already knows the cited atom's full
        // source line, so the citation must surface it.
        let citation = output
            .lines()
            .find(|l| l.starts_with("\t\tsrc/user.rs:12"))
            .expect("refs block must cite src/user.rs:12");
        let raw = citation
            .trim_start_matches('\t')
            .trim_start_matches("src/user.rs:12")
            .trim();
        assert!(
            !raw.is_empty(),
            "citation to a top-level reference atom must surface its known source \
             line, never an empty raw field (bug 48); got blank citation: \
             {citation:?}"
        );
    }

    // ─── Paging ───────────────────────────────────────────────────────

    #[test]
    fn grep_page_header_single_page() {
        let fs = test_fs("/project");
        let hits = [sym_hit(
            "/project/src/handler.rs",
            100,
            "handle_grep",
            "function",
            "fn handle_grep() {",
        )];

        let output = render(&hits, 10_000, 1, &fs);

        assert!(
            !output.contains("[page"),
            "single-page result should have no page header: {output}"
        );
        assert!(
            output.contains("fn handle_grep() {"),
            "should contain content: {output}"
        );
    }

    #[test]
    fn grep_paged() {
        let fs = test_fs("/project");

        let mut hits = Vec::new();
        for i in 0..50 {
            hits.push(sym_hit(
                &format!("/project/src/file_{i}.rs"),
                i * 10,
                &format!("test_symbol_{i}"),
                "function",
                &format!("fn test_symbol_{i}() {{"),
            ));
        }

        let page1 = render(&hits, 200, 1, &fs);
        let page2 = render(&hits, 200, 2, &fs);

        assert!(
            page1.starts_with("[page 1/"),
            "page 1 should have header: {page1}"
        );
        assert!(
            page2.starts_with("[page 2/"),
            "page 2 should have header: {page2}"
        );
        assert_ne!(page1, page2, "page 1 and 2 should differ");
    }

    #[test]
    fn grep_page_beyond_last_clamps() {
        let fs = test_fs("/project");
        let hits = [sym_hit(
            "/project/src/handler.rs",
            100,
            "handle_grep",
            "function",
            "fn handle_grep() {",
        )];

        let output = render(&hits, 10_000, 99, &fs);

        // Beyond-last clamps to last page, no header for single page.
        assert!(
            !output.contains("[page"),
            "single page should have no header: {output}"
        );
        assert!(
            output.contains("fn handle_grep() {"),
            "clamped page should contain content: {output}"
        );
    }

    #[test]
    fn grep_bare_path_headers() {
        let fs = test_fs("/project");
        let hits = [sym_hit(
            "/project/src/handler.rs",
            100,
            "handle_grep",
            "function",
            "fn handle_grep() {",
        )];

        let output = render(&hits, 10_000, 1, &fs);

        assert!(
            !output.contains("Root:"),
            "should not contain Root: prefix: {output}"
        );
        assert!(
            !output.contains("OutOfRoots:"),
            "should not contain OutOfRoots: prefix: {output}"
        );
        assert!(
            output.contains("/project"),
            "should contain bare absolute path: {output}"
        );
    }

    #[test]
    fn grep_out_of_root_hits() {
        let fs = test_fs("/project");
        let hits = [sym_hit(
            "/other/path/file.rs",
            10,
            "orphan_fn",
            "function",
            "fn orphan_fn() {",
        )];

        let output = render(&hits, 10_000, 1, &fs);

        assert!(
            !output.contains("OutOfRoots:"),
            "should not contain OutOfRoots: prefix: {output}"
        );
        assert!(
            output.contains("/other/path"),
            "should contain parent directory path: {output}"
        );
    }

    #[test]
    fn grep_out_of_root_grouped_by_parent() {
        let fs = test_fs("/project");
        let hits = [
            sym_hit("/other/path/a.rs", 10, "fn_a", "function", "fn fn_a() {"),
            sym_hit("/other/path/b.rs", 20, "fn_b", "function", "fn fn_b() {"),
        ];

        let output = render(&hits, 10_000, 1, &fs);

        // Both grouped under one /other/path header, not two
        let header_count = output.matches("/other/path\n").count();
        assert_eq!(
            header_count, 1,
            "expected one /other/path header, got {header_count} in:\n{output}"
        );
    }

    // ─── Enrichment rendering ─────────────────────────────────────────

    #[test]
    fn render_enriched_calls_only() {
        let fs = test_fs("/project");
        let hit = sym_hit(
            "/project/src/lib.rs",
            10,
            "MyStruct",
            "struct",
            "struct MyStruct {",
        );
        let mut enrichment = empty_enrichment();
        enrichment.outgoing_calls.push(Edge {
            name: "helper".to_string(),
            target: AtomId::new("/project/src/util.rs", 5),
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, &fs, None);

        // Top-level atom = the full source line, no `<Kind>`.
        assert!(
            full.starts_with("/project\nsrc/lib.rs:11  struct MyStruct {\n"),
            "definition atom directly under root header: {full:?}"
        );
        assert!(full.contains("\tcalls:\n"), "calls header: {full}");
        // The call target is not a top-level match and its source line is
        // unavailable (no real file) → falls back to the edge name, 1-based.
        assert!(
            full.contains("\t\tsrc/util.rs:6  helper"),
            "call edge atom (citation/name fallback): {full}"
        );
        assert!(!full.contains('<'), "no kind label anywhere: {full}");
    }

    #[test]
    fn render_enriched_impls_only() {
        let fs = test_fs("/project");
        let hit = sym_hit(
            "/project/src/lib.rs",
            10,
            "MyTrait",
            "interface",
            "trait MyTrait {",
        );
        let mut enrichment = empty_enrichment();
        enrichment
            .implementations
            .push(AtomId::new("/project/src/impl.rs", 30));

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, &fs, None);

        assert!(full.contains("\timpls:\n"), "impls header: {full}");
        // Impl target is not a match and has no readable line → bare atom.
        assert!(
            full.contains("\t\tsrc/impl.rs:31"),
            "impl edge atom 1-based: {full}"
        );
    }

    #[test]
    fn render_enriched_supertypes_only() {
        let fs = test_fs("/project");
        let hit = sym_hit(
            "/project/src/lib.rs",
            10,
            "MyStruct",
            "struct",
            "struct MyStruct {",
        );
        let mut enrichment = empty_enrichment();
        enrichment.supertypes.push(Edge {
            name: "BaseTrait".to_string(),
            target: AtomId::new("/project/src/traits.rs", 20),
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, &fs, None);

        assert!(
            full.contains("\tsupertypes:\n"),
            "supertypes header: {full}"
        );
        assert!(
            full.contains("\t\tsrc/traits.rs:21  BaseTrait"),
            "supertype edge atom: {full}"
        );
        assert!(!full.contains('<'), "no kind label: {full}");
    }

    #[test]
    fn render_enriched_subtypes_only() {
        let fs = test_fs("/project");
        let hit = sym_hit(
            "/project/src/lib.rs",
            10,
            "MyTrait",
            "interface",
            "trait MyTrait {",
        );
        let mut enrichment = empty_enrichment();
        enrichment.subtypes.push(Edge {
            name: "SubStruct".to_string(),
            target: AtomId::new("/project/src/sub.rs", 15),
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, &fs, None);

        assert!(full.contains("\tsubtypes:\n"), "subtypes header: {full}");
        assert!(
            full.contains("\t\tsrc/sub.rs:16  SubStruct"),
            "subtype edge atom: {full}"
        );
    }

    #[test]
    fn render_enriched_refs_from_ref_lines_only() {
        let fs = test_fs("/project");
        let hit = sym_hit(
            "/project/src/lib.rs",
            10,
            "MyStruct",
            "struct",
            "struct MyStruct {",
        );
        let mut enrichment = empty_enrichment();
        enrichment
            .ref_lines
            .insert(AtomId::new("/project/src/main.rs", 20));

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, &fs, None);

        assert!(full.contains("\trefs:\n"), "refs header: {full}");
        assert!(
            full.contains("\t\tsrc/main.rs:21"),
            "ref edge atom 1-based: {full}"
        );
    }

    #[test]
    fn render_enriched_refs_from_incoming_calls_only() {
        let fs = test_fs("/project");
        let hit = sym_hit(
            "/project/src/lib.rs",
            10,
            "MyStruct",
            "struct",
            "struct MyStruct {",
        );
        let mut enrichment = empty_enrichment();
        enrichment.incoming_calls.push(Edge {
            name: "caller_fn".to_string(),
            target: AtomId::new("/project/src/caller.rs", 50),
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, &fs, None);

        assert!(full.contains("\trefs:\n"), "refs header: {full}");
        // Incoming caller is not a top-level match; no readable line → name.
        assert!(
            full.contains("\t\tsrc/caller.rs:51  caller_fn"),
            "incoming call edge atom: {full}"
        );
    }

    #[test]
    fn render_edge_to_matched_symbol_collapses_to_citation() {
        // A→B where B is also a top-level match: the edge to B collapses to a
        // `path:line  name` citation (the full line is B's own atom), and B's
        // standalone atom is rendered full. Cycles fall out the same way.
        let fs = test_fs("/project");
        let hit_a = sym_hit("/project/src/lib.rs", 10, "FnA", "function", "fn FnA() {");
        let mut enrichment_a = empty_enrichment();
        enrichment_a.outgoing_calls.push(Edge {
            name: "FnB".to_string(),
            target: AtomId::new("/project/src/util.rs", 20),
        });
        // B at the cited location, a top-level match in its own right.
        let hit_b = sym_hit("/project/src/util.rs", 20, "FnB", "function", "fn FnB() {");

        let enrichments = vec![(&hit_a, Some(enrichment_a)), (&hit_b, None)];
        let full = render_results(&enrichments, &fs, None);

        // A's atom is full, with a calls: group.
        assert!(
            full.contains("src/lib.rs:11  fn FnA() {"),
            "FnA atom full: {full}"
        );
        assert!(full.contains("\tcalls:\n"), "calls section: {full}");
        // The edge to B collapses to a citation (its name), NOT its full line.
        assert!(
            full.contains("\t\tsrc/util.rs:21  FnB"),
            "edge to matched B is a citation: {full}"
        );
        // B is ALSO a top-level atom, rendered full elsewhere.
        assert!(
            full.contains("src/util.rs:21  fn FnB() {"),
            "FnB still rendered full at top level: {full}"
        );
    }

    #[test]
    fn cross_page_citation_is_a_resolvable_pointer() {
        // The paging invariant (decision 024): when a definition and a citing
        // edge land on DIFFERENT pages, the collapsed citation is still an
        // absolute pointer to an atom rendered full elsewhere in the result —
        // nothing is withheld. Here B's full atom and the A→B citation are
        // forced onto separate pages by a tiny budget; both pages remain valid.
        let fs = test_fs("/project");
        let hit_a = sym_hit("/project/a.rs", 0, "FnA", "function", "fn FnA() {");
        let mut enrichment_a = empty_enrichment();
        enrichment_a.outgoing_calls.push(Edge {
            name: "FnB".to_string(),
            target: AtomId::new("/project/b.rs", 0),
        });
        let hit_b = sym_hit("/project/b.rs", 0, "FnB", "function", "fn FnB() {");

        let enrichments = vec![(&hit_a, Some(enrichment_a)), (&hit_b, None)];
        let full = render_results(&enrichments, &fs, None);

        // The full result holds BOTH the citation and B's full atom.
        let citation = "b.rs:1  FnB";
        let full_atom = "b.rs:1  fn FnB() {";
        assert!(full.contains(citation), "citation present: {full}");
        assert!(full.contains(full_atom), "B full atom present: {full}");

        // Page with a tiny budget so the citation and B's full atom are forced
        // onto SEPARATE pages, then walk every page. Paging never withholds:
        // the citation appears on exactly one page, the full atom on exactly
        // one page, and across the page set BOTH survive — the citation is an
        // absolute `path:line` pointer to a guaranteed-present atom, never a
        // dangle, regardless of which page each lands on.
        let mut all_pages = String::new();
        let mut citation_pages = 0;
        let mut full_atom_pages = 0;
        for page in 1..=10 {
            let rendered = paginate(&full, 24, page);
            // Count membership on each page (the page header line is ignored).
            if rendered.contains(citation) {
                citation_pages += 1;
            }
            if rendered.contains(full_atom) {
                full_atom_pages += 1;
            }
            all_pages.push_str(&rendered);
            all_pages.push('\n');
        }
        // Distinct pages: the full atom (the long line) cannot fit on the same
        // 24-char page as the citation, so they are genuinely separated.
        assert!(
            citation_pages >= 1 && full_atom_pages >= 1,
            "both the citation and the full atom appear on some page: {all_pages}"
        );
        // Across the full page set, the citation and its target both survive —
        // nothing withheld, the pointer resolves.
        assert!(
            all_pages.contains(citation) && all_pages.contains(full_atom),
            "across all pages, citation and its target both survive: {all_pages}"
        );
    }

    #[test]
    fn render_multiple_roots_separated_by_blank_line() {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from("/project1"), PathBuf::from("/project2")]);
        let hits = [
            sym_hit("/project1/src/a.rs", 5, "fn_a", "function", "fn fn_a() {"),
            sym_hit("/project2/src/b.rs", 15, "fn_b", "function", "fn fn_b() {"),
        ];

        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> =
            hits.iter().map(|h| (h, None)).collect();
        let full = render_results(&enrichments, &fs, None);

        assert!(!full.starts_with('\n'), "no leading newline: {full:?}");
        assert!(full.contains("/project1\n"), "first root header: {full}");
        assert!(full.contains("/project2\n"), "second root header: {full}");
        assert!(
            full.contains("\n\n/project2"),
            "blank line between root sections: {full:?}"
        );
    }

    #[test]
    fn render_oor_sections_separated_by_blank_line() {
        let fs = test_fs("/project");
        let hits = [
            sym_hit("/other/dir1/a.rs", 5, "fn_a", "function", "fn fn_a() {"),
            sym_hit("/other/dir2/b.rs", 15, "fn_b", "function", "fn fn_b() {"),
        ];

        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> =
            hits.iter().map(|h| (h, None)).collect();
        let full = render_results(&enrichments, &fs, None);

        assert!(full.contains("/other/dir1\n"), "first oor parent: {full}");
        assert!(full.contains("/other/dir2\n"), "second oor parent: {full}");
        assert!(
            full.contains("\n\n/other/dir2"),
            "blank line between oor sections: {full:?}"
        );
    }

    #[test]
    fn render_results_cwd_relative_paths_and_header() {
        let fs = test_fs("/project");
        let hit = sym_hit(
            "/project/src/lib.rs",
            10,
            "MyStruct",
            "struct",
            "struct MyStruct {",
        );
        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> = vec![(&hit, None)];
        let full = render_results(&enrichments, &fs, Some(Path::new("/project")));

        // cwd header present with the path (inside a root: no LSP warning).
        assert!(
            full.starts_with("cwd: /project\n"),
            "cwd header should be first line: {full:?}"
        );
        // Relative path used (no root grouping header)
        assert!(
            full.contains("src/lib.rs:11  struct MyStruct {"),
            "path should be cwd-relative, atom is the full line: {full}"
        );
        // No root grouping header — cwd mode uses a single flat section.
        assert!(
            !full.lines().any(|l| l == "/project"),
            "should not have standalone root header in cwd mode: {full}"
        );
    }

    #[test]
    fn name_embedding_server_has_no_double_prefix() {
        // A name-embedding server (lattice) emits headings whose source line is
        // already clean (`# Title`). With no `<Kind>` rendered there is nothing
        // to double-prefix (bug 29 symptom gone).
        let fs = test_fs("/project");
        let hit = sym_hit("/project/doc.md", 0, "H1: Title", "class", "# Title");
        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> = vec![(&hit, None)];
        let full = render_results(&enrichments, &fs, None);

        assert!(
            full.contains("doc.md:1  # Title"),
            "heading atom is the clean source line: {full}"
        );
        assert!(!full.contains('<'), "no kind label: {full}");
        assert!(
            !full.contains("H1: H1:") && !full.contains("Class"),
            "no double prefix / class label: {full}"
        );
    }

    #[test]
    fn render_is_byte_stable_across_runs() {
        // BTreeMap `(file, line)` ordering → reproducible bytes (decision 012).
        let fs = test_fs("/project");
        let hits = [
            sym_hit("/project/src/b.rs", 4, "fn_b", "function", "fn fn_b() {"),
            sym_hit("/project/src/a.rs", 9, "fn_a", "function", "fn fn_a() {"),
            ref_hit("/project/src/a.rs", 2, "    // fn_a usage"),
        ];
        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> =
            hits.iter().map(|h| (h, None)).collect();
        let a = render_results(&enrichments, &fs, None);
        let b = render_results(&enrichments, &fs, None);
        assert_eq!(a, b, "render must be byte-stable across runs");
        // a.rs:3 (line 2) sorts before a.rs:10 (line 9), both before b.rs.
        let a3 = a.find("a.rs:3").expect("a.rs:3 present");
        let a10 = a.find("a.rs:10").expect("a.rs:10 present");
        let b5 = a.find("b.rs:5").expect("b.rs:5 present");
        assert!(a3 < a10 && a10 < b5, "atoms ordered by (file, line): {a}");
    }

    // ─── Paginate unit tests ──────────────────────────────────────────

    #[test]
    fn paginate_single_page() {
        let output = paginate("line one\nline two", 1000, 1);
        assert!(
            !output.contains("[page"),
            "single page should have no header: {output}"
        );
        assert!(output.contains("line one"), "missing content: {output}");
    }

    #[test]
    fn paginate_multi_page() {
        let output1 = paginate("aaa\nbbb\nccc", 5, 1);
        let output2 = paginate("aaa\nbbb\nccc", 5, 2);
        assert!(output1.starts_with("[page 1/"), "page 1 header: {output1}");
        assert!(output1.contains("aaa"), "page 1 content: {output1}");
        assert!(
            !output1.contains("bbb"),
            "page 1 excludes page 2: {output1}"
        );
        assert!(output2.starts_with("[page 2/"), "page 2 header: {output2}");
        assert!(output2.contains("bbb"), "page 2 content: {output2}");
        assert!(
            !output2.contains("ccc"),
            "page 2 excludes page 3: {output2}"
        );
    }

    #[test]
    fn paginate_beyond_last_clamps() {
        let output = paginate("aaa\nbbb", 1000, 5);
        // Beyond-last clamps to last page and shows content, no header for single page.
        assert!(
            !output.contains("[page"),
            "single page should have no header: {output}"
        );
        assert!(output.contains("aaa"), "clamped page has content: {output}");
    }

    #[test]
    fn paginate_splits_over_budget_not_at_boundary() {
        // "aaaa" (4 chars + 1 newline = 5) + "bbbb" (4 + 1 = 5) = 10 budget chars.
        // Budget 10: both fit → single page (verifies > not >=).
        // Budget 9: second line pushes to 10 > 9 → split.
        let text = "aaaa\nbbbb";

        // At budget: single page (verifies > not >=)
        let at = paginate(text, 10, 1);
        assert!(
            !at.contains("[page"),
            "budget=len should be single page with no header: {at}"
        );
        assert!(at.contains("aaaa"), "page has first line: {at}");
        assert!(at.contains("bbbb"), "page has second line: {at}");

        // Over budget: two pages (verifies newline counted in budget)
        let over1 = paginate(text, 9, 1);
        let over2 = paginate(text, 9, 2);
        assert!(over1.starts_with("[page 1/2]"), "page 1 of 2: {over1}");
        assert!(over1.contains("aaaa"), "page 1 content: {over1}");
        assert!(!over1.contains("bbbb"), "page 1 excludes page 2: {over1}");
        assert!(over2.starts_with("[page 2/2]"), "page 2 of 2: {over2}");
        assert!(over2.contains("bbbb"), "page 2 content: {over2}");
    }

    // ─── extract_location_path ──────────────────────────────────────────

    #[test]
    fn extract_location_path_valid() {
        let loc = serde_json::json!({
            "uri": "file:///home/user/project/src/main.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
        });
        assert_eq!(
            extract_location_path(&loc),
            Some("/home/user/project/src/main.rs".to_string())
        );
    }

    #[test]
    fn extract_location_path_non_file_uri() {
        let loc = serde_json::json!({"uri": "untitled:Untitled-1"});
        assert_eq!(extract_location_path(&loc), None);
    }

    #[test]
    fn extract_location_path_missing_uri() {
        let loc = serde_json::json!({"range": {"start": {"line": 0}}});
        assert_eq!(extract_location_path(&loc), None);
    }

    // ─── extract_start_line ─────────────────────────────────────────────

    #[test]
    fn extract_start_line_valid() {
        let loc = serde_json::json!({
            "range": {"start": {"line": 42, "character": 0}, "end": {"line": 50, "character": 0}}
        });
        assert_eq!(extract_start_line(&loc), Some(42));
    }

    #[test]
    fn extract_start_line_zero() {
        let loc = serde_json::json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
        });
        assert_eq!(extract_start_line(&loc), Some(0));
    }

    #[test]
    fn extract_start_line_missing_range() {
        let loc = serde_json::json!({"uri": "file:///foo.rs"});
        assert_eq!(extract_start_line(&loc), None);
    }

    // ─── extract_edge (one extractor for call- and type-hierarchy items) ──

    #[test]
    fn extract_edge_from_call_hierarchy_item() {
        let item = serde_json::json!({
            "name": "my_function",
            "kind": 12,
            "detail": "MyModule",
            "uri": "file:///project/src/lib.rs",
            "range": {"start": {"line": 10, "character": 4}, "end": {"line": 20, "character": 1}}
        });
        let edge = extract_edge(&item).expect("should parse call edge");
        assert_eq!(edge.name, "my_function");
        assert_eq!(edge.target.file, "/project/src/lib.rs");
        assert_eq!(edge.target.line, 10);
    }

    #[test]
    fn extract_edge_from_type_hierarchy_item() {
        let item = serde_json::json!({
            "name": "MyTrait",
            "kind": 11,
            "detail": "my_crate",
            "uri": "file:///project/src/traits.rs",
            "range": {"start": {"line": 20, "character": 0}, "end": {"line": 30, "character": 1}}
        });
        let edge = extract_edge(&item).expect("should parse type edge");
        assert_eq!(edge.name, "MyTrait");
        assert_eq!(edge.target.file, "/project/src/traits.rs");
        assert_eq!(edge.target.line, 20);
    }

    #[test]
    fn extract_edge_missing_name() {
        let item = serde_json::json!({
            "uri": "file:///project/src/lib.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
        });
        assert!(extract_edge(&item).is_none());
    }

    #[test]
    fn extract_edge_missing_uri() {
        let item = serde_json::json!({
            "name": "no_uri",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
        });
        assert!(extract_edge(&item).is_none());
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
            None,
            false, // include_gitignored = false: the bypass must not depend on it
            false,
            &fs,
        )
        .expect("ripgrep_matches");

        assert!(
            rg.file_lines.keys().any(|k| k.ends_with("ignored.rs")),
            "named gitignored file must be searched without --include-gitignored: {:?}",
            rg.file_lines
        );
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
            None,
            false,
            false,
            &fs,
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
            None,
            true,
            false,
            &fs,
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

    // ─── default_page ───────────────────────────────────────────────────

    #[test]
    fn default_page_is_one() {
        assert_eq!(default_page(), 1);
    }
}
