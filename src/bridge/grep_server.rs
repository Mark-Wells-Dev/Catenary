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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

use super::filesystem_manager::FilesystemManager;
use super::handler::display_path;
use super::pagination::paginate;
use crate::config::DispatchMethod;
use crate::lsp::LspClientManager;
use crate::lsp::server::LspServer;
use crate::source::Source;
use crate::symbol_index::{
    CallEdge, Symbol, SymbolEnrichment, SymbolIndex, TypeEdge, format_symbol_kind,
};

/// Input for grep tool.
#[derive(Debug, Deserialize)]
pub struct GrepInput {
    /// Search pattern (supports `|` for alternation, passed to ripgrep).
    pub pattern: String,
    /// Glob pattern to scope the search (optional).
    #[serde(default)]
    pub glob: Option<String>,
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
    matched_text: String,
    classification: HitClass,
}

/// Classification of a ripgrep hit against the symbol index.
enum HitClass {
    /// rg hit at a symbol index definition line.
    Symbol { symbol: Symbol },
    /// rg hit at a non-definition line, with optional enclosing structure.
    Reference { enclosing: Option<Symbol> },
    /// Symbol identified via `prepareRename` (no symbol index data for file).
    PrepareRenameSymbol,
    /// Keyword filtered out via `prepareRename` (will be dropped).
    Keyword,
}

/// Grep tool server: ripgrep + symbol index pipeline with LSP enrichment.
pub struct GrepServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    pub(super) budget: usize,
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
    ) -> Result<serde_json::Value> {
        let input: GrepInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.pattern.is_empty() {
            return Err(anyhow!("pattern must be non-empty"));
        }

        if input.page == 0 {
            return Err(anyhow!("page must be >= 1"));
        }

        // Relative glob param → cwd context header.
        let cwd = input
            .glob
            .as_ref()
            .filter(|g| !PathBuf::from(g).is_absolute())
            .and_then(|_| std::env::current_dir().ok());

        // Split top-level alternation into independent arms
        let arms = split_alternation(&input.pattern);

        let mut all_output = String::new();
        for arm in &arms {
            let arm_input = GrepInput {
                pattern: arm.clone(),
                glob: input.glob.clone(),
                exclude: input.exclude.clone(),
                include_gitignored: input.include_gitignored,
                include_hidden: input.include_hidden,
                page: input.page,
            };
            let output = self
                .run(arm_input, parent_id, cancel, cwd.as_deref())
                .await?;
            if !output.is_empty() {
                if !all_output.is_empty() {
                    all_output.push('\n');
                }
                all_output.push_str(&output);
            }
        }

        if all_output.is_empty() {
            return Ok(Value::String("No results found".to_string()));
        }

        Ok(Value::String(paginate(
            &all_output,
            self.budget,
            input.page,
        )))
    }

    /// Grep pipeline: ripgrep + `documentSymbol` index + hit classification.
    #[allow(clippy::too_many_lines, reason = "Core grep orchestration")]
    async fn run(
        &self,
        input: GrepInput,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
        cwd: Option<&Path>,
    ) -> Result<String> {
        debug!("Grep request: pattern={}", input.pattern);

        let resolved_glob = input
            .glob
            .as_deref()
            .map(ResolvedGlob::new)
            .transpose()?
            .map(Arc::new);
        let resolved_exclude = input
            .exclude
            .as_deref()
            .map(ResolvedGlob::new)
            .transpose()?
            .map(Arc::new);

        // Determine effective search roots: absolute glob overrides workspace roots.
        let workspace_roots = self.client_manager.roots();
        let effective_roots = if let Some(ref rg) = resolved_glob
            && let Some(override_root) = rg.override_root()
        {
            vec![override_root.to_path_buf()]
        } else {
            workspace_roots
        };

        // Step 1: Ripgrep scoped to file set → raw hits with matched text.
        let rg = Self::ripgrep_matches(
            &input.pattern,
            &effective_roots,
            resolved_glob.as_ref(),
            resolved_exclude.as_ref(),
            input.include_gitignored,
            input.include_hidden,
            &self.fs_manager,
        )?;

        if rg.file_lines.is_empty() {
            return Ok(String::new());
        }

        // Step 2: Ensure servers exist for matched files and wait for readiness.
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

        // Step 2b: Populate symbol index for matched files.
        super::ensure_symbols(self.symbol_index.as_ref(), &self.client_manager, &rg_paths).await;

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

        // Build lookup: (file_path, line) → Symbol for definitions
        let mut def_lookup: HashMap<(String, u32), Symbol> = HashMap::new();
        for (path, sym) in &indexed_symbols {
            let path_str = path.to_string_lossy().to_string();
            def_lookup.insert((path_str, sym.line), sym.clone());
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

                if has_symbols {
                    // Check if this line is a definition
                    if let Some(sym) = def_lookup.get(&(file_str.clone(), line_0)) {
                        hits.push(GrepHit {
                            file: file_path.clone(),
                            line: line_0,
                            col,
                            matched_text: matched_text.clone(),
                            classification: HitClass::Symbol {
                                symbol: sym.clone(),
                            },
                        });
                    } else {
                        // Non-definition line — find enclosing structure via SQL.
                        // find_enclosing opens a throwaway read connection internally.
                        let enclosing = self.symbol_index.as_ref().and_then(|idx| {
                            idx.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .find_enclosing(&file_path, line_0)
                                .ok()
                                .flatten()
                        });
                        hits.push(GrepHit {
                            file: file_path.clone(),
                            line: line_0,
                            col,
                            matched_text,
                            classification: HitClass::Reference { enclosing },
                        });
                    }
                } else {
                    // No symbol index data — check if the language server is alive
                    let lang = self.fs_manager.language_id(&file_path);
                    let server_dead = lang
                        .as_ref()
                        .is_some_and(|l| dead_languages.contains(l.as_str()));

                    if server_dead {
                        // Server unavailable — emit bare reference, skip LSP
                        hits.push(GrepHit {
                            file: file_path.clone(),
                            line: line_0,
                            col,
                            matched_text,
                            classification: HitClass::Reference { enclosing: None },
                        });
                    } else {
                        // Server alive — use prepareRename for keyword discrimination
                        let is_symbol = self
                            .prepare_rename_check(&file_path, line_0, col, parent_id, cancel)
                            .await;
                        if is_symbol {
                            hits.push(GrepHit {
                                file: file_path.clone(),
                                line: line_0,
                                col,
                                matched_text,
                                classification: HitClass::PrepareRenameSymbol,
                            });
                        } else {
                            hits.push(GrepHit {
                                file: file_path.clone(),
                                line: line_0,
                                col,
                                matched_text,
                                classification: HitClass::Keyword,
                            });
                        }
                    }
                }
            }
        }

        // Drop keywords
        hits.retain(|h| !matches!(h.classification, HitClass::Keyword));

        if hits.is_empty() {
            return Ok(String::new());
        }

        // Enrich definition-like hits (Symbol, PrepareRenameSymbol).
        // Reference hits pass through with no enrichment.
        let mut enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> = Vec::new();
        for hit in &hits {
            let (line_0, col) = match &hit.classification {
                HitClass::Symbol { symbol } => (symbol.line, hit.col),
                HitClass::PrepareRenameSymbol => (hit.line, hit.col),
                _ => {
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

        Ok(render_results(
            &enrichments,
            self.symbol_index.as_ref(),
            &self.fs_manager,
            cwd,
        ))
    }

    /// Checks `prepareRename` at a position to distinguish symbols from keywords.
    ///
    /// Uses priority chain dispatch: iterates servers that support rename
    /// in binding order, returns on the first definitive answer. Dispatch
    /// errors are logged via `debug!()` and never surface in the tool result.
    ///
    /// Returns `true` if the position is a symbol (or no capable server
    /// exists), `false` if keyword.
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
                Ok(v) if v.is_null() => return false, // null → keyword
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
    /// Callers are responsible for keyword filtering before calling this method
    /// — `GrepServer::run` uses `prepare_rename_check` during hit classification
    /// and only passes confirmed symbols here.
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
        if resolved_root.is_some()
            && let Some(ref idx_arc) = self.symbol_index
            && let Ok(mut idx) = idx_arc.lock()
            && let Some(cached) = idx.get_enrichment(path, line_0, col, &self.fs_manager)
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
            idx.cache_enrichment(path, line_0, col, root, generation, enrichment.clone());
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
    ) -> HashMap<String, HashSet<u32>> {
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
                    let mut ref_lines: HashMap<String, HashSet<u32>> = HashMap::new();
                    for r in &refs {
                        if let Some(file) = extract_location_path(r)
                            && let Some(line) = extract_start_line(r)
                        {
                            ref_lines.entry(file).or_default().insert(line);
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

        HashMap::new()
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
    ) -> (Vec<CallEdge>, Vec<CallEdge>) {
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
                            .filter_map(|c| extract_call_edge(c.get("from")?))
                            .collect(),
                        _ => Vec::new(),
                    };
                    let outgoing = match client.outgoing_calls(item).await {
                        Ok(Value::Array(calls)) => calls
                            .iter()
                            .filter_map(|c| extract_call_edge(c.get("to")?))
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
    ) -> Vec<(String, u32)> {
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
                            Some((file, line))
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
    ) -> (Vec<TypeEdge>, Vec<TypeEdge>) {
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
                        Ok(Value::Array(types)) => {
                            types.iter().filter_map(extract_type_edge).collect()
                        }
                        _ => Vec::new(),
                    };
                    let subtypes = match client.subtypes(item).await {
                        Ok(Value::Array(types)) => {
                            types.iter().filter_map(extract_type_edge).collect()
                        }
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
        glob: Option<&Arc<ResolvedGlob>>,
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
            let walker = WalkBuilder::new(root)
                .git_ignore(skip_gitignored)
                .hidden(skip_hidden)
                .build_parallel();

            walker.run(|| {
                let matcher = matcher.clone();
                let glob = glob.cloned();
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
                    if !path.is_file() {
                        return WalkState::Continue;
                    }

                    if let Some(rg) = &glob
                        && !rg.is_match(path, &root)
                    {
                        return WalkState::Continue;
                    }
                    if let Some(rg) = &exclude
                        && rg.is_match(path, &root)
                    {
                        return WalkState::Continue;
                    }

                    // Skip binary files — no meaningful text matches
                    if let Ok(metadata) = path.metadata()
                        && fs_manager.is_binary(path, &metadata)
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

        let parts = Arc::into_inner(collected)
            .ok_or_else(|| anyhow!("walker threads still hold references"))?
            .into_inner()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;

        Ok(RipgrepMatches::merge(parts))
    }
}

// ─── Rendering ─────────────────────────────────────────────────────────

/// Renders grep results with page-based paging.
///
/// Each hit is rendered with whatever data is available: enriched
/// navigation edges when LSP data exists, enclosing symbol context
/// when available, bare line numbers otherwise. Grouped by workspace
/// root (bare absolute path header) for absolute patterns, or under
/// a `cwd = …` context header for relative glob scoping.
///
/// Returns the full unpaginated output. Pagination is applied by the
/// caller (`execute`).
fn render_results(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    fs_manager: &FilesystemManager,
    cwd: Option<&Path>,
) -> String {
    use std::fmt::Write;

    let mut full = String::new();

    if let Some(cwd) = cwd {
        // Relative glob: one section, cwd context header, cwd-relative paths.
        let _ = writeln!(full, "cwd = {}", cwd.display());
        let all_indices: Vec<usize> = (0..enrichments.len()).collect();
        render_section(
            enrichments,
            &all_indices,
            symbol_index,
            fs_manager,
            &mut full,
            Some(cwd),
        );
    } else {
        // Absolute / no glob: group by workspace root.
        let mut root_items: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        let mut oor_items: Vec<usize> = Vec::new();
        for (i, (hit, _)) in enrichments.iter().enumerate() {
            match fs_manager.resolve_root(&hit.file) {
                Some(root) => root_items.entry(root).or_default().push(i),
                None => oor_items.push(i),
            }
        }

        for (root, indices) in &root_items {
            if !full.is_empty() {
                full.push('\n');
            }
            let _ = writeln!(full, "{}", root.display());
            render_section(
                enrichments,
                indices,
                symbol_index,
                fs_manager,
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
                symbol_index,
                fs_manager,
                &mut full,
                None,
            );
        }
    }

    let trimmed_len = full.trim_end().len();
    full.truncate(trimmed_len);
    full
}

/// Renders a single root section: definitions with enrichment edges,
/// then remaining reference hits with enclosing context.
#[allow(
    clippy::too_many_lines,
    reason = "Renders navigation sections per symbol + reference fallback"
)]
fn render_section(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
    indices: &[usize],
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    fs_manager: &FilesystemManager,
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

    // Step 1: Group by bare name at depth 0.
    let mut by_name: BTreeMap<String, Vec<(&GrepHit, &Option<SymbolEnrichment>)>> = BTreeMap::new();
    for &i in indices {
        let (hit, enrichment) = &enrichments[i];
        let name = match &hit.classification {
            HitClass::Symbol { symbol } => symbol.name.clone(),
            _ => hit.matched_text.clone(),
        };
        by_name.entry(name).or_default().push((hit, enrichment));
    }

    // Step 2: Cross-definition dedup.
    //
    // Suppress definitions whose location appears in any labeled section
    // (calls, impls, supertypes, subtypes) of another enriched definition.
    // If the agent already sees a location in a labeled section, repeating
    // it as a standalone definition is noise.
    //
    // Cycle guard: if A appears in B's labeled section AND B appears in
    // A's, neither is suppressed — they're peers, not parent/child.
    let suppressed: HashSet<(String, u32)> = {
        // Map each labeled location → its dominator (the definition whose
        // section lists it). First writer wins — if two definitions both
        // list the same location, the first one encountered dominates.
        let mut labeled_locs: HashMap<(String, u32), (String, u32)> = HashMap::new();
        for &i in indices {
            let (hit, enrichment) = &enrichments[i];
            let Some(e) = enrichment else { continue };
            let hit_file = hit.file.to_string_lossy().to_string();
            let hit_line = match &hit.classification {
                HitClass::Symbol { symbol } => symbol.line,
                _ => hit.line,
            };
            let dominator = (hit_file, hit_line);
            for c in &e.outgoing_calls {
                labeled_locs
                    .entry((c.file.clone(), c.line))
                    .or_insert_with(|| dominator.clone());
            }
            for (f, l) in &e.implementations {
                labeled_locs
                    .entry((f.clone(), *l))
                    .or_insert_with(|| dominator.clone());
            }
            for t in &e.supertypes {
                labeled_locs
                    .entry((t.file.clone(), t.line))
                    .or_insert_with(|| dominator.clone());
            }
            for t in &e.subtypes {
                labeled_locs
                    .entry((t.file.clone(), t.line))
                    .or_insert_with(|| dominator.clone());
            }
        }

        // Only suppress if the dominator is not itself dominated (no cycles).
        labeled_locs
            .keys()
            .filter(|loc| {
                let Some(dom) = labeled_locs.get(loc) else {
                    return true;
                };
                !labeled_locs.contains_key(dom)
            })
            .cloned()
            .collect()
    };

    // Step 4: Render each name group.

    for (name, group) in &by_name {
        let visible: Vec<&(&GrepHit, &Option<SymbolEnrichment>)> = group
            .iter()
            .filter(|(hit, _)| {
                let hit_file = hit.file.to_string_lossy().to_string();
                let hit_line = match &hit.classification {
                    HitClass::Symbol { symbol } => symbol.line,
                    _ => hit.line,
                };
                !suppressed.contains(&(hit_file, hit_line))
            })
            .collect();

        if visible.is_empty() {
            continue;
        }

        // Blank line between name groups.
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }

        // Render definition-like hits (Symbol, PrepareRenameSymbol) with
        // enrichment edges.
        for (hit, enrichment) in &visible {
            let has_edges = enrichment.as_ref().is_some_and(|e| {
                !e.outgoing_calls.is_empty()
                    || !e.implementations.is_empty()
                    || !e.supertypes.is_empty()
                    || !e.subtypes.is_empty()
                    || !e.ref_lines.is_empty()
                    || !e.incoming_calls.is_empty()
            });

            let rel_path = rel(&hit.file.to_string_lossy());

            // Definition line — only Symbol and PrepareRenameSymbol hits
            match &hit.classification {
                HitClass::Symbol { symbol } => {
                    let kind = format_symbol_kind(&symbol.kind);
                    let scope_prefix = symbol
                        .scope
                        .as_ref()
                        .zip(symbol.scope_kind.as_ref())
                        .map_or_else(String::new, |(sn, sk)| {
                            format!("<{}> {}/", format_symbol_kind(sk), sn)
                        });
                    let line_1 = symbol.line + 1;
                    let _ = writeln!(output, "{scope_prefix}<{kind}> {name}  {rel_path}:{line_1}");
                }
                HitClass::PrepareRenameSymbol => {
                    let line_1 = hit.line + 1;
                    let _ = writeln!(output, "{name}  {rel_path}:{line_1}");
                }
                _ => continue,
            }

            // Fish-eye: symbols with no edges → lean single line (already rendered).
            if !has_edges {
                continue;
            }

            let Some(enrichment) = enrichment else {
                continue;
            };

            // Build the set of labeled (file, line) pairs for ref dedup
            let mut this_labeled: HashSet<(String, u32)> = HashSet::new();
            for c in &enrichment.outgoing_calls {
                this_labeled.insert((c.file.clone(), c.line));
            }
            for (f, l) in &enrichment.implementations {
                this_labeled.insert((f.clone(), *l));
            }
            for t in &enrichment.supertypes {
                this_labeled.insert((t.file.clone(), t.line));
            }
            for t in &enrichment.subtypes {
                this_labeled.insert((t.file.clone(), t.line));
            }

            // calls: section — outgoing calls sorted alphabetically
            if !enrichment.outgoing_calls.is_empty() {
                let _ = writeln!(output, "\tcalls:");
                let mut calls: Vec<&CallEdge> = enrichment.outgoing_calls.iter().collect();
                calls.sort_by(|a, b| a.name.cmp(&b.name));
                for c in &calls {
                    let kind_label = crate::symbol_index::lsp_kind_label(c.kind);
                    let depr = if c.deprecated { ", deprecated" } else { "" };
                    let container_prefix = c.container.as_ref().map_or_else(String::new, |cn| {
                        // Look up container kind from symbol_index if available
                        let ck = symbol_index
                            .and_then(|idx| {
                                let index = idx
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let path = PathBuf::from(&c.file);
                                index.find_enclosing(&path, c.line).ok().flatten()
                            })
                            .map_or_else(String::new, |enc| {
                                format!("<{}> ", format_symbol_kind(&enc.kind))
                            });
                        format!("{ck}{cn}/")
                    });
                    let c_rel = rel(&c.file);
                    let line_1 = c.line + 1;
                    let _ = writeln!(
                        output,
                        "\t\t{container_prefix}<{kind_label}{depr}> {}  {c_rel}:{line_1}",
                        c.name
                    );
                }
            }

            // impls: section — grouped by file (alphabetical)
            if !enrichment.implementations.is_empty() {
                let _ = writeln!(output, "\timpls:");
                let mut by_file: BTreeMap<String, Vec<u32>> = BTreeMap::new();
                for (f, l) in &enrichment.implementations {
                    by_file.entry(f.clone()).or_default().push(*l);
                }
                for (file, lines) in &by_file {
                    let mut lines = lines.clone();
                    lines.sort_unstable();
                    let f_rel = rel(file);
                    let _ = writeln!(output, "\t\t{f_rel}");
                    for line_0 in &lines {
                        let line_1 = line_0 + 1;
                        // Look up enclosing structure from symbol_index
                        let enc_str = symbol_index
                            .and_then(|idx| {
                                let index = idx
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let path = PathBuf::from(file);
                                index.find_enclosing(&path, *line_0).ok().flatten()
                            })
                            .map_or_else(String::new, |enc| {
                                let ek = format_symbol_kind(&enc.kind);
                                let span = format_span(enc.line, enc.end_line);
                                format!(" <{ek}> {}{span}", enc.name)
                            });
                        let _ = writeln!(output, "\t\t\t:{line_1}{enc_str}");
                    }
                }
            }

            // supertypes: section
            if !enrichment.supertypes.is_empty() {
                let _ = writeln!(output, "\tsupertypes:");
                for t in &enrichment.supertypes {
                    let kind_label = crate::symbol_index::lsp_kind_label(t.kind);
                    let depr = if t.deprecated { ", deprecated" } else { "" };
                    let container_prefix = t
                        .container
                        .as_ref()
                        .map_or_else(String::new, |cn| format!("{cn}/"));
                    let t_rel = rel(&t.file);
                    let line_1 = t.line + 1;
                    let _ = writeln!(
                        output,
                        "\t\t{container_prefix}<{kind_label}{depr}> {}  {t_rel}:{line_1}",
                        t.name
                    );
                }
            }

            // subtypes: section
            if !enrichment.subtypes.is_empty() {
                let _ = writeln!(output, "\tsubtypes:");
                for t in &enrichment.subtypes {
                    let kind_label = crate::symbol_index::lsp_kind_label(t.kind);
                    let depr = if t.deprecated { ", deprecated" } else { "" };
                    let container_prefix = t
                        .container
                        .as_ref()
                        .map_or_else(String::new, |cn| format!("{cn}/"));
                    let t_rel = rel(&t.file);
                    let line_1 = t.line + 1;
                    let _ = writeln!(
                        output,
                        "\t\t{container_prefix}<{kind_label}{depr}> {}  {t_rel}:{line_1}",
                        t.name
                    );
                }
            }

            // refs: section — merge incoming calls, dedup against labeled sections
            let mut ref_entries: BTreeMap<String, BTreeMap<u32, Option<Symbol>>> = BTreeMap::new();

            // Add textDocument/references lines
            for (file, lines) in &enrichment.ref_lines {
                for &line_0 in lines {
                    if this_labeled.contains(&(file.clone(), line_0)) {
                        continue;
                    }
                    let enc = symbol_index.and_then(|idx| {
                        let index = idx
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let path = PathBuf::from(file);
                        index.find_enclosing(&path, line_0).ok().flatten()
                    });
                    ref_entries
                        .entry(file.clone())
                        .or_default()
                        .insert(line_0, enc);
                }
            }

            // Merge incoming calls into refs (dedup: same file + same line)
            for caller in &enrichment.incoming_calls {
                if this_labeled.contains(&(caller.file.clone(), caller.line)) {
                    continue;
                }
                // Use the caller's line as the ref entry. Dedup: if already present, skip.
                let file_entries = ref_entries.entry(caller.file.clone()).or_default();
                file_entries.entry(caller.line).or_insert_with(|| {
                    symbol_index.and_then(|idx| {
                        let index = idx
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let path = PathBuf::from(&caller.file);
                        index.find_enclosing(&path, caller.line).ok().flatten()
                    })
                });
            }

            if !ref_entries.is_empty() {
                let _ = writeln!(output, "\trefs:");
                for (file, lines) in &ref_entries {
                    let f_rel = rel(file);
                    let _ = writeln!(output, "\t\t{f_rel}");
                    for (&line_0, enc) in lines {
                        let line_1 = line_0 + 1;
                        let enc_str = enc.as_ref().map_or_else(String::new, |enc| {
                            let ek = format_symbol_kind(&enc.kind);
                            let scope_prefix = enc
                                .scope
                                .as_ref()
                                .zip(enc.scope_kind.as_ref())
                                .map_or_else(String::new, |(sn, sk)| {
                                    format!("<{}> {}/", format_symbol_kind(sk), sn)
                                });
                            let span = format_span(enc.line, enc.end_line);
                            format!(" {scope_prefix}<{ek}> {}{span}", enc.name)
                        });
                        let _ = writeln!(output, "\t\t\t:{line_1}{enc_str}");
                    }
                }
            }
        }

        // Remaining Reference hits: not definition-like, rendered with
        // enclosing context in dir/file grouping below definitions.
        let ref_hits: Vec<&GrepHit> = visible
            .iter()
            .filter(|(hit, _)| matches!(hit.classification, HitClass::Reference { .. }))
            .map(|(hit, _)| *hit)
            .collect();
        if !ref_hits.is_empty() {
            let by_dir_file = group_hits_by_dir_file(&ref_hits, fs_manager, cwd);
            for (dir, files) in &by_dir_file {
                if !dir.is_empty() {
                    let _ = writeln!(output, "{dir}");
                }
                for (file, file_hits) in files {
                    let indent = if dir.is_empty() { "" } else { "\t" };
                    let _ = writeln!(output, "{indent}{file}");
                    for hit in file_hits {
                        let line_1 = hit.line + 1;
                        let hit_indent = if dir.is_empty() { "\t" } else { "\t\t" };
                        let _ = writeln!(output, "{hit_indent}{}", format_hit_line(hit, line_1));
                    }
                }
            }
        }
    }
}

/// Groups hits by directory and file for tree rendering.
fn group_hits_by_dir_file<'a>(
    hits: &[&'a GrepHit],
    fs_manager: &FilesystemManager,
    cwd: Option<&Path>,
) -> BTreeMap<String, BTreeMap<String, Vec<&'a GrepHit>>> {
    let mut by_dir_file: BTreeMap<String, BTreeMap<String, Vec<&GrepHit>>> = BTreeMap::new();
    for hit in hits {
        let display = cwd.map_or_else(
            || display_path(&hit.file.to_string_lossy(), fs_manager),
            |base| {
                hit.file.strip_prefix(base).map_or_else(
                    |_| hit.file.to_string_lossy().to_string(),
                    |r| r.to_string_lossy().to_string(),
                )
            },
        );
        let (dir, file) = split_dir_file(&display);
        by_dir_file
            .entry(dir)
            .or_default()
            .entry(file)
            .or_default()
            .push(hit);
    }
    by_dir_file
}

/// Formats a single hit line with enclosing structure.
///
/// For definition hits: `:line <Kind> name:start-end`
/// For reference hits with enclosing: `:line <Kind> enclosing:start-end`
/// For bare hits: `:line`
fn format_hit_line(hit: &GrepHit, line_1: u32) -> String {
    match &hit.classification {
        HitClass::Symbol { symbol } => {
            let kind = format_symbol_kind(&symbol.kind);
            let scope_prefix = symbol
                .scope
                .as_ref()
                .zip(symbol.scope_kind.as_ref())
                .map_or_else(String::new, |(sn, sk)| {
                    format!("<{}> {}/", format_symbol_kind(sk), sn)
                });
            let span = format_span(symbol.line, symbol.end_line);
            format!(":{line_1} {scope_prefix}<{kind}> {}{span}", symbol.name)
        }
        HitClass::Reference {
            enclosing: Some(enc),
        } => {
            let enc_kind = format_symbol_kind(&enc.kind);
            let scope_prefix = enc
                .scope
                .as_ref()
                .zip(enc.scope_kind.as_ref())
                .map_or_else(String::new, |(sn, sk)| {
                    format!("<{}> {}/", format_symbol_kind(sk), sn)
                });
            let span = format_span(enc.line, enc.end_line);
            format!(
                ":{line_1} {}  {scope_prefix}<{enc_kind}> {}{span}",
                hit.matched_text, enc.name
            )
        }
        HitClass::Reference { enclosing: None } | HitClass::PrepareRenameSymbol => {
            format!(":{line_1} {}", hit.matched_text)
        }
        HitClass::Keyword => String::new(),
    }
}

/// Formats a span: `:start-end` for multi-line, `:line` for single-line.
fn format_span(start_0: u32, end_0: u32) -> String {
    let start_1 = start_0 + 1;
    let end_1 = end_0 + 1;
    if start_1 == end_1 {
        format!(":{start_1}")
    } else {
        format!(":{start_1}-{end_1}")
    }
}

/// Splits a relative path into `(directory/, filename)`.
///
/// `"src/bridge/handler.rs"` → `("src/bridge/", "handler.rs")`
/// `"handler.rs"` → `("", "handler.rs")`
fn split_dir_file(rel: &str) -> (String, String) {
    rel.rfind('/').map_or_else(
        || (String::new(), rel.to_string()),
        |pos| (format!("{}/", &rel[..pos]), rel[pos + 1..].to_string()),
    )
}

/// Wrapper that pushes per-thread match data into a shared collector on drop.
/// Each parallel walker thread owns one of these; when `run()` returns and the
/// closures are dropped, each thread's accumulated matches are flushed.
struct CollectOnDrop {
    local: ThreadMatches,
    collected: Arc<std::sync::Mutex<Vec<ThreadMatches>>>,
}

impl Drop for CollectOnDrop {
    fn drop(&mut self) {
        let local = std::mem::take(&mut self.local);
        if local.file_lines.is_empty() {
            return;
        }
        if let Ok(mut vec) = self.collected.lock() {
            vec.push(local);
        }
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

        // Extract each individual match from the line (--only-matching equivalent).
        let mut at = 0;
        while let Ok(Some(m)) = self.matcher.find_at(line_bytes, at) {
            if m.start() == m.end() {
                // Zero-width match — advance to avoid infinite loop
                at = m.end() + 1;
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&line_bytes[m]) {
                let expanded = expand_match_to_token(line_bytes, m.start(), m.end());
                let col = u32::try_from(m.start()).unwrap_or(0);
                self.local
                    .file_line_texts
                    .entry(self.path.to_string())
                    .or_default()
                    .entry(line_num)
                    .or_default()
                    .push((expanded.unwrap_or_else(|| text.to_string()), col));
            }
            at = m.end();
        }

        self.local
            .file_lines
            .entry(self.path.to_string())
            .or_default()
            .push(line_num);

        Ok(true)
    }
}

// ─── Alternation splitting ────────────────────────────────────────────

/// Result of a ripgrep `--only-matching` search.
#[derive(Default)]
struct RipgrepMatches {
    /// Per-file line numbers.
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line matched texts with column offsets
    /// `(matched_text, column_byte_offset)` for hit classification
    /// and for no-grammar `prepareRename` positions.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
}

impl RipgrepMatches {
    /// Merges per-thread match accumulators into a single result.
    fn merge(parts: Vec<ThreadMatches>) -> Self {
        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>> = HashMap::new();

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
        }

        Self {
            file_lines,
            file_line_texts,
        }
    }
}

/// Per-thread match accumulator used during parallel file walking.
#[derive(Default)]
struct ThreadMatches {
    /// Per-file line numbers.
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line matched texts with column offsets.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
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

// ─── Match expansion ────────────────────────────────────────────────────

/// Returns `true` if a byte is a token delimiter.
///
/// Token delimiters are whitespace and common punctuation that separate
/// identifiers in source code. The match text is expanded to the nearest
/// delimiters on each side so reference hits show the full token, not a
/// regex substring (e.g. `Configuration` instead of `Config`).
const fn is_token_delimiter(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t'
            | b'\n'
            | b'\r'
            | b'('
            | b')'
            | b'['
            | b']'
            | b'<'
            | b'>'
            | b'{'
            | b'}'
            | b','
            | b';'
            | b':'
            | b'.'
            | b'='
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'!'
            | b'?'
            | b'&'
            | b'|'
            | b'^'
            | b'~'
            | b'#'
            | b'@'
            | b'%'
            | b'"'
            | b'\''
            | b'`'
    )
}

/// Expands a regex match span to token boundaries within a line.
///
/// Walks left from `start` and right from `end` until a delimiter or
/// line boundary is reached. Returns the expanded substring, or `None`
/// if the bytes aren't valid UTF-8.
fn expand_match_to_token(line: &[u8], start: usize, end: usize) -> Option<String> {
    let mut lo = start;
    while lo > 0 && !is_token_delimiter(line[lo - 1]) {
        lo -= 1;
    }
    let mut hi = end;
    while hi < line.len() && !is_token_delimiter(line[hi]) {
        hi += 1;
    }
    std::str::from_utf8(&line[lo..hi]).ok().map(str::to_string)
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

/// Extracts a [`CallEdge`] from a `CallHierarchyItem` JSON value.
fn extract_call_edge(item: &Value) -> Option<CallEdge> {
    let name = item.get("name")?.as_str()?.to_string();
    let kind = u32::try_from(item.get("kind")?.as_u64()?).ok()?;
    let container = item
        .get("detail")
        .and_then(Value::as_str)
        .map(str::to_string);
    let file = item
        .get("uri")?
        .as_str()?
        .strip_prefix("file://")
        .map(str::to_string)?;
    let line = u32::try_from(item.get("range")?.get("start")?.get("line")?.as_u64()?).ok()?;
    let deprecated = item
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| tags.iter().any(|t| t.as_u64() == Some(1)));
    Some(CallEdge {
        name,
        kind,
        container,
        file,
        line,
        deprecated,
    })
}

/// Extracts a [`TypeEdge`] from a `TypeHierarchyItem` JSON value.
fn extract_type_edge(item: &Value) -> Option<TypeEdge> {
    let name = item.get("name")?.as_str()?.to_string();
    let kind = u32::try_from(item.get("kind")?.as_u64()?).ok()?;
    let container = item
        .get("detail")
        .and_then(Value::as_str)
        .map(str::to_string);
    let file = item
        .get("uri")?
        .as_str()?
        .strip_prefix("file://")
        .map(str::to_string)?;
    let line = u32::try_from(item.get("range")?.get("start")?.get("line")?.as_u64()?).ok()?;
    let deprecated = item
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| tags.iter().any(|t| t.as_u64() == Some(1)));
    Some(TypeEdge {
        name,
        kind,
        container,
        file,
        line,
        deprecated,
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

    // ─── Tier rendering helpers ─────────────────────────────────────────

    /// Build a `GrepHit` with a `Symbol` classification for testing.
    fn sym_hit(file: &str, line: u32, name: &str, kind: &str) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            col: 0,
            matched_text: name.to_string(),
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

    /// Build a `GrepHit` with a `Symbol` that has scope (enclosing container).
    fn scoped_sym_hit(
        file: &str,
        line: u32,
        name: &str,
        kind: &str,
        scope: &str,
        scope_kind: &str,
    ) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            col: 0,
            matched_text: name.to_string(),
            classification: HitClass::Symbol {
                symbol: Symbol {
                    name: name.to_string(),
                    kind: kind.to_string(),
                    line,
                    end_line: line + 10,
                    scope: Some(scope.to_string()),
                    scope_kind: Some(scope_kind.to_string()),
                    deprecated: false,
                },
            },
        }
    }

    /// Build a `GrepHit` with a `Reference` classification with enclosing.
    fn ref_hit(
        file: &str,
        line: u32,
        text: &str,
        enc_name: &str,
        enc_kind: &str,
        enc_start: u32,
        enc_end: u32,
    ) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            col: 0,
            matched_text: text.to_string(),
            classification: HitClass::Reference {
                enclosing: Some(Symbol {
                    name: enc_name.to_string(),
                    kind: enc_kind.to_string(),
                    line: enc_start,
                    end_line: enc_end,
                    scope: None,
                    scope_kind: None,
                    deprecated: false,
                }),
            },
        }
    }

    /// Build a `GrepHit` with a bare `Reference` (no enclosing).
    fn bare_ref_hit(file: &str, line: u32, text: &str) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            col: 0,
            matched_text: text.to_string(),
            classification: HitClass::Reference { enclosing: None },
        }
    }

    /// Build a `GrepHit` with `PrepareRenameSymbol` (no-grammar path).
    fn prepare_rename_hit(file: &str, line: u32, text: &str) -> GrepHit {
        GrepHit {
            file: PathBuf::from(file),
            line,
            col: 0,
            matched_text: text.to_string(),
            classification: HitClass::PrepareRenameSymbol,
        }
    }

    fn test_fs(root: &str) -> FilesystemManager {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from(root)]);
        fs
    }

    fn empty_enrichment() -> SymbolEnrichment {
        SymbolEnrichment {
            ref_lines: HashMap::new(),
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
        let full = render_results(&enrichments, None, fs, None);
        paginate(&full, budget, page)
    }

    #[test]
    fn test_name_grouping() {
        let fs = test_fs("/project");
        let hits = [
            sym_hit(
                "/project/tests/a.rs",
                287,
                "test_glob_directory",
                "function",
            ),
            sym_hit(
                "/project/tests/b.rs",
                118,
                "test_glob_directory",
                "function",
            ),
            sym_hit("/project/src/handler.rs", 1085, "test_glob", "function"),
        ];

        let output = render(&hits, 10_000, 1, &fs);

        assert!(
            output.contains("test_glob_directory"),
            "missing name group: {output}"
        );
        assert!(output.contains("test_glob"), "missing name group: {output}");
        assert!(
            output.contains("<Function>"),
            "missing kind label: {output}"
        );
    }

    #[test]
    fn test_mixed_definitions_and_references() {
        let fs = test_fs("/project");
        // Same matched text: one PrepareRenameSymbol (definition-like)
        // and one Reference. Both should appear.
        let hits = [
            prepare_rename_hit("/project/data/config.yaml", 15, "handle"),
            ref_hit(
                "/project/src/util.rs",
                30,
                "handle",
                "process",
                "function",
                25,
                50,
            ),
        ];

        let output = render(&hits, 10_000, 1, &fs);

        // Definition rendered
        assert!(
            output.contains("config.yaml:16"),
            "definition should be present: {output}"
        );
        // Reference also rendered (not dropped)
        assert!(
            output.contains("util.rs"),
            "reference should not be dropped: {output}"
        );
        assert!(
            output.contains(":31"),
            "reference line should be present: {output}"
        );
    }

    #[test]
    fn test_plain_references_only() {
        let fs = test_fs("/project");
        let hits = [
            bare_ref_hit("/project/data/notes.txt", 5, "pattern"),
            ref_hit(
                "/project/src/main.rs",
                100,
                "pattern",
                "call_tool",
                "function",
                95,
                120,
            ),
        ];

        let output = render(&hits, 10_000, 1, &fs);

        // Both references rendered with 1-based lines
        assert!(output.contains(":6"), "bare ref line: {output}");
        assert!(output.contains(":101"), "enclosing ref line: {output}");
        assert!(output.contains("call_tool"), "enclosing name: {output}");
        // Directory headers present for non-empty dirs
        assert!(output.contains("data/\n"), "data dir header: {output}");
        assert!(output.contains("src/\n"), "src dir header: {output}");
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
        )];

        let output = render(&hits, 10_000, 1, &fs);

        assert!(
            output.starts_with("[page 1/1]"),
            "single-page result should show [page 1/1]: {output}"
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
        )];

        let output = render(&hits, 10_000, 99, &fs);

        // Beyond-last clamps to last page and still shows content.
        assert!(
            output.starts_with("[page 1/1]"),
            "beyond-last should clamp to last page: {output}"
        );
        assert!(
            output.contains("handle_grep"),
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
        let hits = [sym_hit("/other/path/file.rs", 10, "orphan_fn", "function")];

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
            sym_hit("/other/path/a.rs", 10, "fn_a", "function"),
            sym_hit("/other/path/b.rs", 20, "fn_b", "function"),
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
        let hit = sym_hit("/project/src/lib.rs", 10, "MyStruct", "struct");
        let mut enrichment = empty_enrichment();
        enrichment.outgoing_calls.push(CallEdge {
            name: "helper".to_string(),
            kind: 12,
            container: None,
            file: "/project/src/util.rs".to_string(),
            line: 5,
            deprecated: false,
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, None, &fs, None);

        assert!(
            full.starts_with("/project\n<"),
            "no blank line between root header and definition: {full:?}"
        );
        assert!(
            full.contains("<Struct> MyStruct  src/lib.rs:11"),
            "definition with 1-based line: {full}"
        );
        assert!(full.contains("\tcalls:\n"), "calls header: {full}");
        assert!(
            full.contains("<Fn> helper  src/util.rs:6"),
            "call edge with 1-based line: {full}"
        );
    }

    #[test]
    fn render_enriched_impls_only() {
        let fs = test_fs("/project");
        let hit = sym_hit("/project/src/lib.rs", 10, "MyTrait", "interface");
        let mut enrichment = empty_enrichment();
        enrichment
            .implementations
            .push(("/project/src/impl.rs".to_string(), 30));

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, None, &fs, None);

        assert!(full.contains("\timpls:\n"), "impls header: {full}");
        assert!(full.contains("\t\tsrc/impl.rs\n"), "impl file: {full}");
        assert!(full.contains("\t\t\t:31"), "impl 1-based line: {full}");
    }

    #[test]
    fn render_enriched_supertypes_only() {
        let fs = test_fs("/project");
        let hit = sym_hit("/project/src/lib.rs", 10, "MyStruct", "struct");
        let mut enrichment = empty_enrichment();
        enrichment.supertypes.push(TypeEdge {
            name: "BaseTrait".to_string(),
            kind: 11,
            container: None,
            file: "/project/src/traits.rs".to_string(),
            line: 20,
            deprecated: false,
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, None, &fs, None);

        assert!(
            full.contains("\tsupertypes:\n"),
            "supertypes header: {full}"
        );
        assert!(
            full.contains("<Iface> BaseTrait  src/traits.rs:21"),
            "supertype with 1-based line: {full}"
        );
    }

    #[test]
    fn render_enriched_subtypes_only() {
        let fs = test_fs("/project");
        let hit = sym_hit("/project/src/lib.rs", 10, "MyTrait", "interface");
        let mut enrichment = empty_enrichment();
        enrichment.subtypes.push(TypeEdge {
            name: "SubStruct".to_string(),
            kind: 23,
            container: None,
            file: "/project/src/sub.rs".to_string(),
            line: 15,
            deprecated: false,
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, None, &fs, None);

        assert!(full.contains("\tsubtypes:\n"), "subtypes header: {full}");
        assert!(
            full.contains("<Struct> SubStruct  src/sub.rs:16"),
            "subtype with 1-based line: {full}"
        );
    }

    #[test]
    fn render_enriched_refs_from_ref_lines_only() {
        let fs = test_fs("/project");
        let hit = sym_hit("/project/src/lib.rs", 10, "MyStruct", "struct");
        let mut enrichment = empty_enrichment();
        enrichment
            .ref_lines
            .insert("/project/src/main.rs".to_string(), HashSet::from([20]));

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, None, &fs, None);

        assert!(full.contains("\trefs:\n"), "refs header: {full}");
        assert!(full.contains("\t\tsrc/main.rs\n"), "ref file: {full}");
        assert!(full.contains("\t\t\t:21"), "ref 1-based line: {full}");
    }

    #[test]
    fn render_enriched_refs_from_incoming_calls_only() {
        let fs = test_fs("/project");
        let hit = sym_hit("/project/src/lib.rs", 10, "MyStruct", "struct");
        let mut enrichment = empty_enrichment();
        enrichment.incoming_calls.push(CallEdge {
            name: "caller_fn".to_string(),
            kind: 12,
            container: None,
            file: "/project/src/caller.rs".to_string(),
            line: 50,
            deprecated: false,
        });

        let enrichments = vec![(&hit, Some(enrichment))];
        let full = render_results(&enrichments, None, &fs, None);

        assert!(full.contains("\trefs:\n"), "refs header: {full}");
        assert!(
            full.contains("\t\tsrc/caller.rs\n"),
            "incoming call file: {full}"
        );
        assert!(
            full.contains("\t\t\t:51"),
            "incoming call 1-based line: {full}"
        );
    }

    #[test]
    fn render_cross_def_dedup_suppresses_dominated() {
        let fs = test_fs("/project");
        // A has outgoing call to B's location — B is dominated
        let hit_a = sym_hit("/project/src/lib.rs", 10, "FnA", "function");
        let mut enrichment_a = empty_enrichment();
        enrichment_a.outgoing_calls.push(CallEdge {
            name: "FnB".to_string(),
            kind: 12,
            container: None,
            file: "/project/src/util.rs".to_string(),
            line: 20,
            deprecated: false,
        });
        // B at the dominated location, no enrichment
        let hit_b = sym_hit("/project/src/util.rs", 20, "FnB", "function");

        let enrichments = vec![(&hit_a, Some(enrichment_a)), (&hit_b, None)];
        let full = render_results(&enrichments, None, &fs, None);

        // A rendered as definition with calls section
        assert!(
            full.contains("<Function> FnA  src/lib.rs:11"),
            "FnA definition: {full}"
        );
        assert!(full.contains("\tcalls:\n"), "calls section: {full}");
        // B suppressed as standalone definition (still in A's calls section)
        let standalone_b = full
            .lines()
            .filter(|l| l.starts_with("<Function> FnB"))
            .count();
        assert_eq!(
            standalone_b, 0,
            "FnB should be suppressed as standalone: {full}"
        );
    }

    #[test]
    fn render_multiple_roots_separated_by_blank_line() {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from("/project1"), PathBuf::from("/project2")]);
        let hits = [
            sym_hit("/project1/src/a.rs", 5, "fn_a", "function"),
            sym_hit("/project2/src/b.rs", 15, "fn_b", "function"),
        ];

        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> =
            hits.iter().map(|h| (h, None)).collect();
        let full = render_results(&enrichments, None, &fs, None);

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
            sym_hit("/other/dir1/a.rs", 5, "fn_a", "function"),
            sym_hit("/other/dir2/b.rs", 15, "fn_b", "function"),
        ];

        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> =
            hits.iter().map(|h| (h, None)).collect();
        let full = render_results(&enrichments, None, &fs, None);

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
        let hit = sym_hit("/project/src/lib.rs", 10, "MyStruct", "struct");
        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> = vec![(&hit, None)];
        let full = render_results(&enrichments, None, &fs, Some(Path::new("/project")));

        // cwd header present with the path
        assert!(
            full.starts_with("cwd = /project\n"),
            "cwd header should be first line: {full:?}"
        );
        // Relative path used (no root grouping header)
        assert!(
            full.contains("src/lib.rs:11"),
            "path should be cwd-relative: {full}"
        );
        // No root grouping header — cwd mode uses a single flat section.
        // A standalone root header would be a line containing only the path.
        assert!(
            !full.lines().any(|l| l == "/project"),
            "should not have standalone root header in cwd mode: {full}"
        );
    }

    #[test]
    fn render_groups_by_symbol_name_not_matched_text() {
        let fs = test_fs("/project");
        // matched_text differs from symbol.name (partial ripgrep match)
        let hit = GrepHit {
            file: PathBuf::from("/project/src/lib.rs"),
            line: 10,
            col: 0,
            matched_text: "MyStr".to_string(),
            classification: HitClass::Symbol {
                symbol: Symbol {
                    name: "MyStruct".to_string(),
                    kind: "struct".to_string(),
                    line: 10,
                    end_line: 20,
                    scope: None,
                    scope_kind: None,
                    deprecated: false,
                },
            },
        };

        let enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> = vec![(&hit, None)];
        let full = render_results(&enrichments, None, &fs, None);

        assert!(
            full.contains("<Struct> MyStruct"),
            "should use symbol.name for display: {full}"
        );
        assert!(
            !full.contains("<Struct> MyStr "),
            "should not use matched_text for display: {full}"
        );
    }

    // ─── Paginate unit tests ──────────────────────────────────────────

    #[test]
    fn paginate_single_page() {
        let output = paginate("line one\nline two", 1000, 1);
        assert!(
            output.starts_with("[page 1/1]"),
            "expected single page header: {output}"
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
        // Beyond-last clamps to last page and shows content.
        assert!(
            output.starts_with("[page 1/1]"),
            "beyond-last should clamp: {output}"
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
            at.starts_with("[page 1/1]"),
            "budget=len should be single page: {at}"
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

    // ─── format_hit_line tests ──────────────────────────────────────────

    #[test]
    fn test_single_line_structure() {
        // Single-line symbol (start == end) should show `:line` not `:start-end`
        let hit = GrepHit {
            file: PathBuf::from("/project/src/main.rs"),
            line: 42,
            col: 0,
            matched_text: "CONST_VAL".to_string(),
            classification: HitClass::Symbol {
                symbol: Symbol {
                    name: "CONST_VAL".to_string(),
                    kind: "constant".to_string(),
                    line: 42,
                    end_line: 42, // single-line
                    scope: None,
                    scope_kind: None,
                    deprecated: false,
                },
            },
        };

        let formatted = format_hit_line(&hit, 43);

        // `:43 <Constant> CONST_VAL:43` — no range
        assert!(
            formatted.contains(":43 <Constant> CONST_VAL:43"),
            "got: {formatted}"
        );
        assert!(
            !formatted.contains('-'),
            "single-line should not have range dash: {formatted}"
        );
    }

    #[test]
    fn test_multi_line_structure() {
        let hit = GrepHit {
            file: PathBuf::from("/project/src/main.rs"),
            line: 10,
            col: 0,
            matched_text: "my_func".to_string(),
            classification: HitClass::Symbol {
                symbol: Symbol {
                    name: "my_func".to_string(),
                    kind: "function".to_string(),
                    line: 10,
                    end_line: 30,
                    scope: None,
                    scope_kind: None,
                    deprecated: false,
                },
            },
        };

        let formatted = format_hit_line(&hit, 11);

        assert!(
            formatted.contains(":11 <Function> my_func:11-31"),
            "got: {formatted}"
        );
    }

    #[test]
    fn test_scoped_symbol_path_syntax() {
        let hit = scoped_sym_hit(
            "/project/src/handler.rs",
            297,
            "handle_grep",
            "method",
            "LspBridgeHandler",
            "implementation",
        );

        let formatted = format_hit_line(&hit, 298);

        // Should use `/`-separated path syntax with scope
        assert!(
            formatted.contains("<Impl> LspBridgeHandler/<Method> handle_grep"),
            "expected path syntax, got: {formatted}"
        );
    }

    // ─── split_dir_file ────────────────────────────────────────────────

    #[test]
    fn test_split_dir_file_nested() {
        assert_eq!(
            split_dir_file("src/bridge/handler.rs"),
            ("src/bridge/".to_string(), "handler.rs".to_string())
        );
    }

    #[test]
    fn test_split_dir_file_root() {
        assert_eq!(
            split_dir_file("handler.rs"),
            (String::new(), "handler.rs".to_string())
        );
    }

    // ─── is_token_delimiter ─────────────────────────────────────────────

    #[test]
    fn token_delimiter_whitespace_and_punctuation() {
        for &b in b" \t\n\r()[]<>{},;:.=+-*/!?&|^~#@%\"'`" {
            assert!(
                is_token_delimiter(b),
                "expected delimiter for {:?}",
                b as char
            );
        }
    }

    #[test]
    fn token_delimiter_identifier_chars_are_not_delimiters() {
        for &b in b"aZ09_" {
            assert!(
                !is_token_delimiter(b),
                "expected non-delimiter for {:?}",
                b as char
            );
        }
    }

    // ─── expand_match_to_token ──────────────────────────────────────────

    #[test]
    fn expand_match_mid_token() {
        let line = b"  hello_world  ";
        let result = expand_match_to_token(line, 3, 8);
        assert_eq!(result.as_deref(), Some("hello_world"));
    }

    #[test]
    fn expand_match_at_line_start() {
        let line = b"Config = value";
        let result = expand_match_to_token(line, 0, 3); // "Con"
        assert_eq!(result.as_deref(), Some("Config"));
    }

    #[test]
    fn expand_match_at_line_end() {
        let line = b"let x = Config";
        let result = expand_match_to_token(line, 10, 14);
        assert_eq!(result.as_deref(), Some("Config"));
    }

    #[test]
    fn expand_match_full_token_between_delimiters() {
        let line = b"(Config)";
        let result = expand_match_to_token(line, 1, 7);
        assert_eq!(result.as_deref(), Some("Config"));
    }

    #[test]
    fn expand_match_entire_line_is_token() {
        let line = b"Configuration";
        let result = expand_match_to_token(line, 0, 6); // "Config" → "Configuration"
        assert_eq!(result.as_deref(), Some("Configuration"));
    }

    #[test]
    fn expand_match_delimiters_on_both_sides() {
        let line = b"foo.bar.baz";
        let result = expand_match_to_token(line, 4, 7); // "bar"
        assert_eq!(result.as_deref(), Some("bar"));
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

    // ─── extract_call_edge ──────────────────────────────────────────────

    #[test]
    fn extract_call_edge_full() {
        let item = serde_json::json!({
            "name": "my_function",
            "kind": 12,
            "detail": "MyModule",
            "uri": "file:///project/src/lib.rs",
            "range": {"start": {"line": 10, "character": 4}, "end": {"line": 20, "character": 1}}
        });
        let edge = extract_call_edge(&item).expect("should parse call edge");
        assert_eq!(edge.name, "my_function");
        assert_eq!(edge.kind, 12);
        assert_eq!(edge.container.as_deref(), Some("MyModule"));
        assert_eq!(edge.file, "/project/src/lib.rs");
        assert_eq!(edge.line, 10);
        assert!(!edge.deprecated);
    }

    #[test]
    fn extract_call_edge_deprecated_tag() {
        let item = serde_json::json!({
            "name": "old_fn",
            "kind": 12,
            "uri": "file:///project/src/lib.rs",
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 10}},
            "tags": [1]
        });
        let edge = extract_call_edge(&item).expect("should parse");
        assert!(edge.deprecated, "tag [1] marks deprecated");
        assert_eq!(edge.container, None);
    }

    #[test]
    fn extract_call_edge_non_deprecated_tags() {
        let item = serde_json::json!({
            "name": "fn_with_tags",
            "kind": 6,
            "uri": "file:///project/src/lib.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
            "tags": [2, 3]
        });
        let edge = extract_call_edge(&item).expect("should parse");
        assert!(!edge.deprecated, "tags [2,3] are not deprecated");
    }

    #[test]
    fn extract_call_edge_missing_name() {
        let item = serde_json::json!({
            "kind": 12,
            "uri": "file:///project/src/lib.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
        });
        assert!(extract_call_edge(&item).is_none());
    }

    // ─── extract_type_edge ──────────────────────────────────────────────

    #[test]
    fn extract_type_edge_full() {
        let item = serde_json::json!({
            "name": "MyTrait",
            "kind": 11,
            "detail": "my_crate",
            "uri": "file:///project/src/traits.rs",
            "range": {"start": {"line": 20, "character": 0}, "end": {"line": 30, "character": 1}}
        });
        let edge = extract_type_edge(&item).expect("should parse type edge");
        assert_eq!(edge.name, "MyTrait");
        assert_eq!(edge.kind, 11);
        assert_eq!(edge.container.as_deref(), Some("my_crate"));
        assert_eq!(edge.file, "/project/src/traits.rs");
        assert_eq!(edge.line, 20);
        assert!(!edge.deprecated);
    }

    #[test]
    fn extract_type_edge_deprecated_tag() {
        let item = serde_json::json!({
            "name": "OldType",
            "kind": 5,
            "uri": "file:///project/src/types.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 1}},
            "tags": [1]
        });
        let edge = extract_type_edge(&item).expect("should parse");
        assert!(edge.deprecated, "tag [1] marks deprecated");
    }

    #[test]
    fn extract_type_edge_non_deprecated_tags() {
        let item = serde_json::json!({
            "name": "TypeWithTags",
            "kind": 5,
            "uri": "file:///project/src/types.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
            "tags": [2]
        });
        let edge = extract_type_edge(&item).expect("should parse");
        assert!(!edge.deprecated, "tag [2] is not deprecated");
    }

    #[test]
    fn extract_type_edge_missing_uri() {
        let item = serde_json::json!({
            "name": "Orphan",
            "kind": 5,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
        });
        assert!(extract_type_edge(&item).is_none());
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

    // ─── default_page ───────────────────────────────────────────────────

    #[test]
    fn default_page_is_one() {
        assert_eq!(default_page(), 1);
    }
}
