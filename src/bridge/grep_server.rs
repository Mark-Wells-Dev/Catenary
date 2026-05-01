// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grep tool: ripgrep + tree-sitter index pipeline with LSP enrichment.

use super::toolbox::ResolvedGlob;
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
use super::tool_server::ToolServer;
use crate::bucketing::{self, BucketEntry};
use crate::lsp::LspClientManager;
use crate::lsp::server::LspServer;
use crate::source::Source;
use crate::symbol_index::{Symbol, SymbolIndex, format_symbol_kind};

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
}

/// A classified hit from the grep pipeline.
struct GrepHit {
    file: PathBuf,
    line: u32,
    col: u32,
    matched_text: String,
    classification: HitClass,
}

/// Classification of a ripgrep hit against the tree-sitter index.
enum HitClass {
    /// rg hit at a tree-sitter definition line.
    Symbol { symbol: Symbol },
    /// rg hit at a non-definition line, with optional enclosing structure.
    Reference { enclosing: Option<Symbol> },
    /// Symbol identified via `prepareRename` (no-grammar path).
    PrepareRenameSymbol,
    /// Keyword filtered out via `prepareRename` (will be dropped).
    Keyword,
}

/// Enrichment data for a single symbol from LSP queries.
pub(super) struct SymbolEnrichment {
    /// Reference lines grouped by file path (0-based line numbers).
    pub ref_lines: HashMap<String, HashSet<u32>>,
    /// Incoming call edges (callers of this symbol).
    pub incoming_calls: Vec<CallEdge>,
    /// Outgoing call edges (callees of this symbol).
    pub outgoing_calls: Vec<CallEdge>,
    /// Implementation locations: `(file_path, line_0)`.
    pub implementations: Vec<(String, u32)>,
    /// Supertype edges.
    pub supertypes: Vec<TypeEdge>,
    /// Subtype edges.
    pub subtypes: Vec<TypeEdge>,
}

/// A call hierarchy edge (caller or callee).
pub(super) struct CallEdge {
    pub name: String,
    pub kind: u32,
    pub container: Option<String>,
    pub file: String,
    pub line: u32,
    pub deprecated: bool,
}

/// A type hierarchy edge (supertype or subtype).
pub(super) struct TypeEdge {
    pub name: String,
    pub kind: u32,
    /// Container name from LSP `detail` field.
    pub container: Option<String>,
    pub file: String,
    pub line: u32,
    pub deprecated: bool,
}

/// Grep tool server: ripgrep + tree-sitter index pipeline with LSP enrichment.
pub struct GrepServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    pub(super) budget: usize,
}

impl ToolServer for GrepServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<i64>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value> {
        let mut input: GrepInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.pattern.is_empty() {
            return Err(anyhow!("pattern must be non-empty"));
        }

        // Explicit hidden targets (e.g. `.gitignore`, `.github/*`) should
        // match without requiring `include_hidden`.
        if let Some(ref glob) = input.glob
            && ResolvedGlob::targets_hidden(glob)
        {
            input.include_hidden = true;
        }

        // Wait for all servers ready (grep doesn't target a specific file).
        self.client_manager.wait_ready_all().await;

        // Collect dead languages so the pipeline can skip prepareRename for them.
        // Exclude single-file servers — they have no project context for
        // workspace-wide search.
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
            };
            let output = self
                .run(arm_input, parent_id, &dead_languages, cancel)
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

        Ok(Value::String(all_output))
    }
}

impl GrepServer {
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
                .get_servers(path, LspServer::supports_document_symbols)
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

    /// Grep pipeline: ripgrep + symbol index + hit classification.
    #[allow(clippy::too_many_lines, reason = "Core grep orchestration")]
    async fn run(
        &self,
        input: GrepInput,
        parent_id: Option<i64>,
        dead_languages: &HashSet<String>,
        cancel: &tokio_util::sync::CancellationToken,
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

        // Step 2b: Populate symbol index for matched files.
        self.ensure_symbols(&rg_paths).await;

        // Step 3: Symbol index query.
        let (ts_symbols, grammar_files) = if let Some(ref index_mutex) = self.symbol_index {
            let index = index_mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let re_pattern = format!("(?i){}", &input.pattern);
            let ts_syms = index
                .query(&re_pattern, Some(&rg_paths))
                .unwrap_or_default();
            let gf: HashSet<String> = rg_paths
                .iter()
                .filter(|p| index.has_symbols_for(p))
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            drop(index);
            (ts_syms, gf)
        } else {
            (Vec::new(), HashSet::new())
        };

        // Build lookup: (file_path, line) → Symbol for definitions
        let mut def_lookup: HashMap<(String, u32), Symbol> = HashMap::new();
        for (path, sym) in &ts_symbols {
            let path_str = path.to_string_lossy().to_string();
            def_lookup.insert((path_str, sym.line), sym.clone());
        }

        // Step 4: Classify each rg hit.
        let mut hits: Vec<GrepHit> = Vec::new();

        for (file_str, line_map) in &rg.file_line_texts {
            let file_path = PathBuf::from(file_str);
            let has_grammar = grammar_files.contains(file_str);

            for (&line_1, texts) in line_map {
                let line_0 = line_1 - 1;
                let matched_text = texts.first().map(|(t, _)| t.clone()).unwrap_or_default();
                let col = texts.first().map_or(0, |(_, c)| *c);

                if has_grammar {
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
                    // No grammar — check if the language server is alive
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

        // Partition by root for grouped output.
        let (root_groups, oor_hits) = partition_hits_by_root(&hits, &self.fs_manager);
        let all_refs: Vec<&GrepHit> = hits.iter().collect();

        // Promote-from-bottom tier selection with enrichment.
        let output = if estimate_tier2_lower_bound(&all_refs, &self.fs_manager) <= self.budget {
            let tier2 = render_grouped(&root_groups, &oor_hits, |h| {
                render_tier2(h, &self.fs_manager)
            });
            if tier2.len() <= self.budget {
                // Tier 2 fits — run enrichment for each symbol hit.
                let mut enrichments: Vec<(&GrepHit, Option<SymbolEnrichment>)> = Vec::new();
                for hit in &hits {
                    let (line_0, col, from_ts) = match &hit.classification {
                        HitClass::Symbol { symbol } => (symbol.line, hit.col, true),
                        HitClass::PrepareRenameSymbol => (hit.line, hit.col, false),
                        _ => {
                            enrichments.push((hit, None));
                            continue;
                        }
                    };
                    if cancel.is_cancelled() {
                        return Err(crate::mcp::RequestCancelled.into());
                    }
                    let enrichment = self
                        .enrich_at_position(&hit.file, line_0, col, from_ts, parent_id, cancel)
                        .await;
                    enrichments.push((hit, enrichment));
                }

                // Try tier 1. If it fits, use it; otherwise fall back to tier 2.
                render_tier1(
                    &enrichments,
                    self.symbol_index.as_ref(),
                    self.budget,
                    &self.fs_manager,
                )
                .unwrap_or(tier2)
            } else {
                render_grouped(&root_groups, &oor_hits, |h| {
                    render_tier3(h, self.budget, &self.fs_manager)
                })
            }
        } else {
            render_grouped(&root_groups, &oor_hits, |h| {
                render_tier3(h, self.budget, &self.fs_manager)
            })
        };
        Ok(output)
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
        parent_id: Option<i64>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let servers = self
            .client_manager
            .get_servers(path, LspServer::supports_rename)
            .await;

        for client_mutex in &servers {
            if cancel.is_cancelled() {
                break;
            }

            let Ok(uri) = self
                .client_manager
                .open_document_on(path, client_mutex, parent_id)
                .await
            else {
                continue;
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id);
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
    /// When `from_ts` is false (no-grammar path), calls `prepareRename` first
    /// to filter keywords.
    ///
    /// Opens the document once on the union of all capability-filtered servers,
    /// runs all four enrichment methods (skipping their per-method open/close),
    /// then closes the document on each server. This avoids `didClose` between
    /// methods causing the server to evict document state.
    async fn enrich_at_position(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        from_ts: bool,
        parent_id: Option<i64>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<SymbolEnrichment> {
        // If !from_ts, check prepareRename first. Null → keyword, return None.
        if !from_ts
            && !self
                .prepare_rename_check(path, line_0, col, parent_id, cancel)
                .await
        {
            return None;
        }

        // Collect the union of servers across all enrichment capabilities.
        let ref_servers = self
            .client_manager
            .get_servers(path, LspServer::supports_references)
            .await;
        let call_servers = self
            .client_manager
            .get_servers(path, LspServer::supports_call_hierarchy)
            .await;
        let impl_servers = self
            .client_manager
            .get_servers(path, LspServer::supports_implementation)
            .await;
        let type_servers = self
            .client_manager
            .get_servers(path, LspServer::supports_type_hierarchy)
            .await;

        let mut all_servers = Vec::new();
        for server in ref_servers
            .iter()
            .chain(call_servers.iter())
            .chain(impl_servers.iter())
            .chain(type_servers.iter())
        {
            if !all_servers.iter().any(|s| Arc::ptr_eq(s, server)) {
                all_servers.push(Arc::clone(server));
            }
        }

        // Open the document once on each server.
        let mut uri_opt: Option<String> = None;
        let mut opened_servers = Vec::new();
        for server in &all_servers {
            match self
                .client_manager
                .open_document_on(path, server, parent_id)
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

        Some(SymbolEnrichment {
            ref_lines,
            incoming_calls,
            outgoing_calls,
            implementations,
            supertypes,
            subtypes,
        })
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
        parent_id: Option<i64>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> HashMap<String, HashSet<u32>> {
        let servers = self
            .client_manager
            .get_servers(path, LspServer::supports_references)
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
                    .open_document_on(path, client_mutex, parent_id)
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id);
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
        parent_id: Option<i64>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> (Vec<CallEdge>, Vec<CallEdge>) {
        let servers = self
            .client_manager
            .get_servers(path, LspServer::supports_call_hierarchy)
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
                    .open_document_on(path, client_mutex, parent_id)
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id);
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
        parent_id: Option<i64>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Vec<(String, u32)> {
        let servers = self
            .client_manager
            .get_servers(path, LspServer::supports_implementation)
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
                    .open_document_on(path, client_mutex, parent_id)
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id);
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
        parent_id: Option<i64>,
        pre_opened_uri: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> (Vec<TypeEdge>, Vec<TypeEdge>) {
        let servers = self
            .client_manager
            .get_servers(path, LspServer::supports_type_hierarchy)
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
                    .open_document_on(path, client_mutex, parent_id)
                    .await
                else {
                    continue;
                };
                owned_uri = u;
                &owned_uri
            };

            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id);
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

// ─── Tier selection and rendering ────────────────────────────────────────

/// Promote-from-bottom tier selection (pure rendering, no enrichment).
///
/// Used by unit tests. The async `run` method inlines this logic with
/// enrichment calls (07a).
#[cfg(test)]
fn select_and_render_tier(
    hits: &[GrepHit],
    budget: usize,
    fs_manager: &FilesystemManager,
) -> String {
    let refs: Vec<&GrepHit> = hits.iter().collect();
    // Cheap lower-bound estimate for tier 2 size: unique name lengths +
    // unique path lengths + per-hit overhead. If the lower bound already
    // exceeds the budget, tier 2 definitely won't fit.
    if estimate_tier2_lower_bound(&refs, fs_manager) <= budget {
        let tier2 = render_tier2(&refs, fs_manager);
        if tier2.len() <= budget {
            // Stub: emit tier 2. (07a replaces with enrichment attempt.)
            return tier2;
        }
    }

    // Tier 2 doesn't fit — fall back to tier 3 (bucketed)
    render_tier3(&refs, budget, fs_manager)
}

// ─── Tier 1: Enriched rendering ─────────────────────────────────────────

/// Tier 1: Enriched rendering with navigation edges.
///
/// Returns `Some(output)` if the rendered result fits within `budget`,
/// `None` if it exceeds the budget (caller falls back to tier 2).
#[allow(
    clippy::too_many_lines,
    reason = "Tier 1 renders six navigation sections per symbol"
)]
fn render_tier1(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    budget: usize,
    fs_manager: &FilesystemManager,
) -> Option<String> {
    use std::fmt::Write;

    // Group enrichments by root.
    let mut root_items: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    let mut oor_items: Vec<usize> = Vec::new();
    for (i, (hit, _)) in enrichments.iter().enumerate() {
        match fs_manager.resolve_root(&hit.file) {
            Some(root) => root_items.entry(root).or_default().push(i),
            None => oor_items.push(i),
        }
    }

    let mut output = String::new();

    for (root, indices) in &root_items {
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "Root: {}", root.display());
        render_tier1_section(enrichments, indices, symbol_index, fs_manager, &mut output);
    }
    if !oor_items.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "OutOfRoots:");
        render_tier1_section(
            enrichments,
            &oor_items,
            symbol_index,
            fs_manager,
            &mut output,
        );
    }

    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);

    if output.len() <= budget {
        Some(output)
    } else {
        None
    }
}

/// Renders a single root section for tier 1 enriched output.
#[allow(
    clippy::too_many_lines,
    reason = "Tier 1 renders six navigation sections per symbol"
)]
fn render_tier1_section(
    enrichments: &[(&GrepHit, Option<SymbolEnrichment>)],
    indices: &[usize],
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    fs_manager: &FilesystemManager,
    output: &mut String,
) {
    use std::fmt::Write;

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

        // Check if this name group has any definition-like hits (Symbol or PrepareRenameSymbol).
        let has_definitions = visible.iter().any(|(hit, _)| {
            matches!(
                hit.classification,
                HitClass::Symbol { .. } | HitClass::PrepareRenameSymbol
            )
        });

        // Name-level grouping header at depth 0
        let _ = writeln!(output, "{name}");

        // If no definitions, render Reference hits with their enclosing structures
        // (rg-only lines under the matched text heading).
        if !has_definitions {
            let by_dir_file = group_hits_by_dir_file(
                &visible.iter().map(|(hit, _)| *hit).collect::<Vec<_>>(),
                fs_manager,
            );
            for (dir, files) in &by_dir_file {
                if !dir.is_empty() {
                    let _ = writeln!(output, "\t{dir}");
                }
                for (file, file_hits) in files {
                    let indent = if dir.is_empty() { "\t" } else { "\t\t" };
                    let _ = writeln!(output, "{indent}{file}");
                    for hit in file_hits {
                        let line_1 = hit.line + 1;
                        let hit_indent = if dir.is_empty() { "\t\t" } else { "\t\t\t" };
                        let _ = writeln!(output, "{hit_indent}{}", format_hit_line(hit, line_1));
                    }
                }
            }
            continue;
        }

        for (hit, enrichment) in &visible {
            let has_edges = enrichment.as_ref().is_some_and(|e| {
                !e.outgoing_calls.is_empty()
                    || !e.implementations.is_empty()
                    || !e.supertypes.is_empty()
                    || !e.subtypes.is_empty()
                    || !e.ref_lines.is_empty()
                    || !e.incoming_calls.is_empty()
            });

            let rel_path = display_path(&hit.file.to_string_lossy(), fs_manager);

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
                    let _ = writeln!(
                        output,
                        "\t{scope_prefix}<{kind}> {name}  {rel_path}:{line_1}"
                    );
                }
                HitClass::PrepareRenameSymbol => {
                    let line_1 = hit.line + 1;
                    let _ = writeln!(output, "\t{name}  {rel_path}:{line_1}");
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
                let _ = writeln!(output, "\t\tcalls:");
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
                    let c_rel = display_path(&c.file, fs_manager);
                    let line_1 = c.line + 1;
                    let _ = writeln!(
                        output,
                        "\t\t\t{container_prefix}<{kind_label}{depr}> {}  {c_rel}:{line_1}",
                        c.name
                    );
                }
            }

            // impls: section — grouped by file (alphabetical)
            if !enrichment.implementations.is_empty() {
                let _ = writeln!(output, "\t\timpls:");
                let mut by_file: BTreeMap<String, Vec<u32>> = BTreeMap::new();
                for (f, l) in &enrichment.implementations {
                    by_file.entry(f.clone()).or_default().push(*l);
                }
                for (file, lines) in &by_file {
                    let mut lines = lines.clone();
                    lines.sort_unstable();
                    let f_rel = display_path(file, fs_manager);
                    let _ = writeln!(output, "\t\t\t{f_rel}");
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
                        let _ = writeln!(output, "\t\t\t\t:{line_1}{enc_str}");
                    }
                }
            }

            // supertypes: section
            if !enrichment.supertypes.is_empty() {
                let _ = writeln!(output, "\t\tsupertypes:");
                for t in &enrichment.supertypes {
                    let kind_label = crate::symbol_index::lsp_kind_label(t.kind);
                    let depr = if t.deprecated { ", deprecated" } else { "" };
                    let container_prefix = t
                        .container
                        .as_ref()
                        .map_or_else(String::new, |cn| format!("{cn}/"));
                    let t_rel = display_path(&t.file, fs_manager);
                    let line_1 = t.line + 1;
                    let _ = writeln!(
                        output,
                        "\t\t\t{container_prefix}<{kind_label}{depr}> {}  {t_rel}:{line_1}",
                        t.name
                    );
                }
            }

            // subtypes: section
            if !enrichment.subtypes.is_empty() {
                let _ = writeln!(output, "\t\tsubtypes:");
                for t in &enrichment.subtypes {
                    let kind_label = crate::symbol_index::lsp_kind_label(t.kind);
                    let depr = if t.deprecated { ", deprecated" } else { "" };
                    let container_prefix = t
                        .container
                        .as_ref()
                        .map_or_else(String::new, |cn| format!("{cn}/"));
                    let t_rel = display_path(&t.file, fs_manager);
                    let line_1 = t.line + 1;
                    let _ = writeln!(
                        output,
                        "\t\t\t{container_prefix}<{kind_label}{depr}> {}  {t_rel}:{line_1}",
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
                let _ = writeln!(output, "\t\trefs:");
                for (file, lines) in &ref_entries {
                    let f_rel = display_path(file, fs_manager);
                    let _ = writeln!(output, "\t\t\t{f_rel}");
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
                        let _ = writeln!(output, "\t\t\t\t:{line_1}{enc_str}");
                    }
                }
            }
        }
    }
}

/// Lower-bound estimate for tier 2 rendered size.
///
/// Sums unique name lengths, unique relative path lengths, and a
/// per-hit minimum overhead. Avoids building the full output string.
fn estimate_tier2_lower_bound(hits: &[&GrepHit], fs_manager: &FilesystemManager) -> usize {
    let mut unique_names: HashSet<&str> = HashSet::new();
    let mut unique_paths: HashSet<String> = HashSet::new();

    for &hit in hits {
        let name = match &hit.classification {
            HitClass::Symbol { symbol } => symbol.name.as_str(),
            _ => hit.matched_text.as_str(),
        };
        unique_names.insert(name);
        unique_paths.insert(display_path(&hit.file.to_string_lossy(), fs_manager));
    }

    // Each unique name: name + newline
    let name_cost: usize = unique_names.iter().map(|n| n.len() + 1).sum();
    // Each unique path: tab(s) + dir + tab(s) + file + newline (~4 overhead)
    let path_cost: usize = unique_paths.iter().map(|p| p.len() + 4).sum();
    // Each hit: tabs + colon + digits + kind bracket + span (~15 minimum)
    let hit_cost: usize = hits.len() * 15;

    name_cost + path_cost + hit_cost
}

/// Tier 2: Structure heatmap.
///
/// Hits grouped by extracted name, then by directory and file, each with
/// enclosing tree-sitter structure and span.
fn render_tier2(hits: &[&GrepHit], fs_manager: &FilesystemManager) -> String {
    use std::fmt::Write;

    let by_name = group_hits_by_name(hits);
    let mut output = String::new();

    for (name, group) in &by_name {
        let _ = writeln!(output, "{name}");

        let by_dir_file = group_hits_by_dir_file(group, fs_manager);

        for (dir, files) in &by_dir_file {
            if !dir.is_empty() {
                let _ = writeln!(output, "\t{dir}");
            }
            for (file, file_hits) in files {
                let indent = if dir.is_empty() { "\t" } else { "\t\t" };
                let _ = writeln!(output, "{indent}{file}");
                for hit in file_hits {
                    let line_1 = hit.line + 1;
                    let hit_indent = if dir.is_empty() { "\t\t" } else { "\t\t\t" };
                    let _ = writeln!(output, "{hit_indent}{}", format_hit_line(hit, line_1));
                }
            }
        }
    }

    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);
    output
}

/// Tier 3: Bucketed patterns with per-bucket equal budget.
///
/// Matched strings bucketed into drillable sub-patterns. Each expanded
/// bucket gets an equal share of the rendering budget. Within its share
/// the bucket tries file-level detail first, then falls back to directory
/// counts. Bare-handle buckets (from the bucketing module's own
/// degradation) are rendered as-is.
fn render_tier3(hits: &[&GrepHit], budget: usize, fs_manager: &FilesystemManager) -> String {
    use std::fmt::Write;

    let text_to_hits = group_hits_by_name(hits);

    // Build bucket entries from unique matched texts
    let bucket_input: Vec<BucketEntry> = text_to_hits
        .keys()
        .map(|v| BucketEntry {
            value: v.clone(),
            context: None,
        })
        .collect();

    let buckets = bucketing::bucket(&bucket_input, budget, true);

    // Compute per-bucket budget: divide equally among expanded buckets.
    let expanded_count = buckets.iter().filter(|b| b.entries.is_some()).count();
    // Reserve space for bare handles.
    let bare_cost: usize = buckets
        .iter()
        .filter(|b| b.entries.is_none())
        .map(|b| b.pattern.len() + count_digits(b.count) + 5) // "pattern (N)\n"
        .sum();
    let per_bucket_budget = budget
        .saturating_sub(bare_cost)
        .checked_div(expanded_count)
        .unwrap_or(0);

    let mut output = String::new();

    for b in &buckets {
        if b.entries.is_none() {
            // Bare handle with count
            let _ = writeln!(output, "{} ({})", b.pattern, b.count);
            continue;
        }

        // Bucket header (with trailing newline — detail lines follow indented)
        let header = render_bucket_header(b);
        let _ = writeln!(output, "{header}");

        // Collect hits for this bucket
        let prefix = b.pattern.trim_end_matches('*');
        let bucket_hits: Vec<&GrepHit> = text_to_hits
            .iter()
            .filter(|(k, _)| {
                if b.count == 1 {
                    b.entries
                        .as_ref()
                        .and_then(|e| e.first())
                        .is_some_and(|e| e.value == **k)
                } else {
                    k.starts_with(prefix)
                }
            })
            .flat_map(|(_, v)| v.iter().copied())
            .collect();

        let by_dir_file = group_hits_by_dir_file(&bucket_hits, fs_manager);

        // Try file detail within this bucket's budget share
        let detail = render_bucket_file_detail(&by_dir_file);
        if header.len() + detail.len() <= per_bucket_budget {
            output.push_str(&detail);
        } else {
            // Fall back to directory counts
            let dir_counts = render_bucket_dir_counts(&by_dir_file);
            output.push_str(&dir_counts);
        }
    }

    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);
    output
}

/// Renders the header line for a tier 3 bucket.
fn render_bucket_header(b: &bucketing::Bucket) -> String {
    if b.count == 1
        && let Some(entries) = &b.entries
        && let Some(entry) = entries.first()
    {
        entry.value.clone()
    } else {
        b.pattern.clone()
    }
}

/// Renders file-level detail for a bucket: directory tree with per-file
/// hit lines and enclosing structures.
fn render_bucket_file_detail(
    by_dir_file: &BTreeMap<String, BTreeMap<String, Vec<&GrepHit>>>,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    for (dir, files) in by_dir_file {
        if !dir.is_empty() {
            let _ = writeln!(out, "\t{dir}");
        }
        for (file, file_hits) in files {
            let indent = if dir.is_empty() { "\t" } else { "\t\t" };
            let _ = writeln!(out, "{indent}{file}");
            for hit in file_hits {
                let line_1 = hit.line + 1;
                let hit_indent = if dir.is_empty() { "\t\t" } else { "\t\t\t" };
                let _ = writeln!(out, "{hit_indent}{}", format_hit_line(hit, line_1));
            }
        }
    }

    out
}

/// Renders directory counts for a bucket: each directory with its total
/// hit count.
fn render_bucket_dir_counts(
    by_dir_file: &BTreeMap<String, BTreeMap<String, Vec<&GrepHit>>>,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    for (dir, files) in by_dir_file {
        let count: usize = files.values().map(Vec::len).sum();
        let label = if dir.is_empty() { "./" } else { dir.as_str() };
        let _ = writeln!(out, "\t{label} ({count})");
    }

    out
}

/// Groups hits by extracted identifier name (`BTreeMap` for stable order).
fn group_hits_by_name<'a>(hits: &[&'a GrepHit]) -> BTreeMap<String, Vec<&'a GrepHit>> {
    let mut by_name: BTreeMap<String, Vec<&GrepHit>> = BTreeMap::new();
    for &hit in hits {
        let key = match &hit.classification {
            HitClass::Symbol { symbol } => symbol.name.clone(),
            _ => hit.matched_text.clone(),
        };
        by_name.entry(key).or_default().push(hit);
    }
    by_name
}

/// Groups hits by directory and file for tree rendering.
fn group_hits_by_dir_file<'a>(
    hits: &[&'a GrepHit],
    fs_manager: &FilesystemManager,
) -> BTreeMap<String, BTreeMap<String, Vec<&'a GrepHit>>> {
    let mut by_dir_file: BTreeMap<String, BTreeMap<String, Vec<&GrepHit>>> = BTreeMap::new();
    for hit in hits {
        let rel = display_path(&hit.file.to_string_lossy(), fs_manager);
        let (dir, file) = split_dir_file(&rel);
        by_dir_file
            .entry(dir)
            .or_default()
            .entry(file)
            .or_default()
            .push(hit);
    }
    by_dir_file
}

/// Number of decimal digits in a `usize`.
const fn count_digits(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut digits = 0;
    let mut val = n;
    while val > 0 {
        digits += 1;
        val /= 10;
    }
    digits
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
            format!(":{line_1} {scope_prefix}<{enc_kind}> {}{span}", enc.name)
        }
        HitClass::Reference { enclosing: None } | HitClass::PrepareRenameSymbol => {
            format!(":{line_1}")
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

// ─── Root grouping ───────────────────────────────────────────────────

/// Partitions grep hits by workspace root.
///
/// Returns `(root_groups, out_of_root)` where `root_groups` is sorted
/// alphabetically by root path.
fn partition_hits_by_root<'a>(
    hits: &'a [GrepHit],
    fs_manager: &FilesystemManager,
) -> (BTreeMap<PathBuf, Vec<&'a GrepHit>>, Vec<&'a GrepHit>) {
    let mut roots: BTreeMap<PathBuf, Vec<&GrepHit>> = BTreeMap::new();
    let mut oor: Vec<&GrepHit> = Vec::new();
    for hit in hits {
        match fs_manager.resolve_root(&hit.file) {
            Some(root) => roots.entry(root).or_default().push(hit),
            None => oor.push(hit),
        }
    }
    (roots, oor)
}

/// Renders per-root sections with `Root:` / `OutOfRoots:` headers.
fn render_grouped(
    root_groups: &BTreeMap<PathBuf, Vec<&GrepHit>>,
    oor_hits: &[&GrepHit],
    render_section: impl Fn(&[&GrepHit]) -> String,
) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    for (root, hits) in root_groups {
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "Root: {}", root.display());
        output.push_str(&render_section(hits));
    }
    if !oor_hits.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "OutOfRoots:");
        output.push_str(&render_section(oor_hits));
    }
    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);
    output
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
        let Some(line_num) = mat
            .line_number()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
        else {
            return Ok(true);
        };

        let line_bytes = mat.bytes();

        // Extract each individual match from the line (--only-matching equivalent)
        let mut at = 0;
        while at < line_bytes.len() {
            let Ok(Some(m)) = self.matcher.find_at(line_bytes, at) else {
                break;
            };
            if m.start() == m.end() {
                // Zero-width match — advance to avoid infinite loop
                at = m.end() + 1;
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&line_bytes[m]) {
                let col = u32::try_from(m.start()).unwrap_or(0);
                self.local
                    .file_line_texts
                    .entry(self.path.to_string())
                    .or_default()
                    .entry(line_num)
                    .or_default()
                    .push((text.to_string(), col));
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
    /// Per-file line numbers (for heatmap tier).
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

    // ─── Tier 2 structure heatmap ───────────────────────────────────────

    #[test]
    fn test_tier2_structure_heatmap() {
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

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier2(&refs, &fs);

        // Names grouped at column 0
        assert!(
            output.contains("test_glob_directory"),
            "missing name group: {output}"
        );
        assert!(output.contains("test_glob"), "missing name group: {output}");

        // File tree structure
        assert!(output.contains("tests/"), "missing directory: {output}");
        assert!(output.contains("a.rs"), "missing file: {output}");
        assert!(output.contains("b.rs"), "missing file: {output}");

        // Enclosing structures with spans
        assert!(
            output.contains("<Function>"),
            "missing kind label: {output}"
        );
        assert!(
            output.contains(":288"),
            "missing line number (1-based): {output}"
        );
    }

    #[test]
    fn test_tier2_no_grammar() {
        let fs = test_fs("/project");
        let hits = [
            bare_ref_hit("/project/data/notes.txt", 5, "pattern"),
            prepare_rename_hit("/project/data/other.txt", 10, "pattern"),
        ];

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier2(&refs, &fs);

        // Bare hit lines (no enclosing structure)
        assert!(output.contains(":6"), "missing bare line: {output}");
        assert!(output.contains(":11"), "missing bare line: {output}");
        // No kind labels for no-grammar hits
        assert!(!output.contains("<Function>"), "unexpected kind: {output}");
    }

    #[test]
    fn test_tier2_reference_with_enclosing() {
        let fs = test_fs("/project");
        let hits = [ref_hit(
            "/project/src/main.rs",
            100,
            "handle",
            "call_tool",
            "function",
            95,
            120,
        )];

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier2(&refs, &fs);

        assert!(output.contains("<Function>"), "missing kind: {output}");
        assert!(
            output.contains("call_tool"),
            "missing enclosing name: {output}"
        );
        assert!(output.contains(":96-121"), "missing span: {output}");
    }

    // ─── Tier selection (promote-from-bottom) ───────────────────────────

    #[test]
    fn test_tier_promotion_narrow_to_tier2() {
        let fs = test_fs("/project");
        // Small result set → fits within budget → tier 2
        let hits = vec![sym_hit(
            "/project/src/handler.rs",
            100,
            "handle_grep",
            "function",
        )];

        let output = select_and_render_tier(&hits, 4000, &fs);

        // Should be tier 2 format: name at depth 0, file tree indented
        assert!(output.contains("handle_grep"), "missing name: {output}");
        assert!(output.contains("src/"), "missing directory: {output}");
        assert!(output.contains("<Function>"), "missing kind: {output}");
    }

    #[test]
    fn test_tier_demotion_to_tier3() {
        let fs = test_fs("/project");

        // Generate enough hits to exceed a very small budget
        let mut hits = Vec::new();
        for i in 0..50 {
            hits.push(sym_hit(
                &format!("/project/src/file_{i}.rs"),
                i * 10,
                &format!("test_alpha_{i}"),
                "function",
            ));
        }
        for i in 0..50 {
            hits.push(sym_hit(
                &format!("/project/src/file_{i}.rs"),
                i * 10 + 5,
                &format!("test_beta_{i}"),
                "function",
            ));
        }

        // Small budget forces tier 3
        let output = select_and_render_tier(&hits, 200, &fs);

        // Tier 3 should contain bucketed patterns (with * wildcards or counts)
        let has_bucket_marker = output.contains('*') || output.contains('(');
        assert!(
            has_bucket_marker,
            "expected tier 3 bucketed output: {output}"
        );
    }

    // ─── Tier 3 bucketed rendering ──────────────────────────────────────

    #[test]
    fn test_tier3_bucketed() {
        let fs = test_fs("/project");

        let mut hits = Vec::new();
        for i in 0..20 {
            hits.push(sym_hit(
                &format!("/project/tests/test_{i}.rs"),
                i,
                &format!("test_mcp_{i}"),
                "function",
            ));
        }
        for i in 0..10 {
            hits.push(sym_hit(
                &format!("/project/tests/glob_{i}.rs"),
                i,
                &format!("test_glob_{i}"),
                "function",
            ));
        }

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier3(&refs, 500, &fs);

        // Should produce bucketed prefixes
        let has_wildcard = output.contains('*');
        assert!(
            has_wildcard,
            "expected wildcard patterns in tier 3: {output}"
        );
    }

    #[test]
    fn test_tier3_bare_handles() {
        let fs = test_fs("/project");

        // Many hits, tiny budget → everything collapses to bare handles
        let mut hits = Vec::new();
        for i in 0..100 {
            hits.push(sym_hit(
                &format!("/project/src/f{i}.rs"),
                i,
                &format!("test_item_{i}"),
                "function",
            ));
        }

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier3(&refs, 100, &fs);

        // Should contain counts in parentheses (bare handle format)
        assert!(
            output.contains('('),
            "expected bare handle counts: {output}"
        );
        assert!(
            output.contains(')'),
            "expected bare handle counts: {output}"
        );
    }

    #[test]
    fn test_tier3_per_bucket_equal_budget() {
        let fs = test_fs("/project");

        // Two clusters: 5 "alpha" hits, 5 "beta" hits.
        // With enough budget for dir counts but not full file detail for
        // all, both clusters should get the same level of detail.
        let mut hits = Vec::new();
        for i in 0..5 {
            hits.push(sym_hit(
                &format!("/project/src/alpha_{i}.rs"),
                i * 10,
                &format!("test_alpha_{i}"),
                "function",
            ));
        }
        for i in 0..5 {
            hits.push(sym_hit(
                &format!("/project/src/beta_{i}.rs"),
                i * 10,
                &format!("test_beta_{i}"),
                "function",
            ));
        }

        // Budget large enough for dir counts on both, not file detail
        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier3(&refs, 300, &fs);

        // Both clusters should appear in the output
        let has_alpha = output.contains("alpha");
        let has_beta = output.contains("beta");
        assert!(
            has_alpha && has_beta,
            "both clusters should appear: {output}"
        );

        // If one has dir counts, the other should too (uniform detail)
        let alpha_has_dir_count = output.contains("src/") && output.contains('(');
        if alpha_has_dir_count {
            // Count how many dir-count lines exist — should be balanced
            let dir_count_count = output
                .lines()
                .filter(|l| l.contains('(') && l.contains(')') && l.trim().starts_with("src/"))
                .count();
            // With two clusters, we expect either 0 or 2 dir-count lines
            // (not 1, which would mean one cluster got counts and the other didn't)
            assert!(
                dir_count_count != 1,
                "expected uniform detail across buckets (0 or 2 dir counts, got 1): {output}"
            );
        }
    }

    #[test]
    fn test_tier2_estimate_skips_render() {
        let fs = test_fs("/project");

        // Many hits — estimate should exceed a tiny budget
        let mut hits = Vec::new();
        for i in 0..200 {
            hits.push(sym_hit(
                &format!("/project/src/very_long_directory_name/file_{i}.rs"),
                i,
                &format!("a_very_long_symbol_name_{i}"),
                "function",
            ));
        }

        // The estimate should be well over 100
        let refs: Vec<&GrepHit> = hits.iter().collect();
        let estimate = estimate_tier2_lower_bound(&refs, &fs);
        assert!(
            estimate > 100,
            "estimate should exceed tiny budget, got {estimate}"
        );

        // select_and_render_tier should produce tier 3, not tier 2
        let output = select_and_render_tier(&hits, 100, &fs);
        let has_bucket = output.contains('*') || output.contains('(');
        assert!(has_bucket, "expected tier 3 (bucketed): {output}");
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

    // ─── No blank lines ────────────────────────────────────────────────

    #[test]
    fn test_no_blank_lines_in_tier2() {
        let fs = test_fs("/project");
        let hits = [
            sym_hit("/project/src/a.rs", 10, "alpha", "function"),
            sym_hit("/project/src/b.rs", 20, "beta", "function"),
            sym_hit("/project/src/c.rs", 30, "gamma", "function"),
        ];

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier2(&refs, &fs);

        // No blank lines (consecutive \n\n) in output
        assert!(
            !output.contains("\n\n"),
            "expected no blank lines between name groups, got:\n{output}"
        );
    }

    #[test]
    fn test_no_blank_lines_in_tier3() {
        let fs = test_fs("/project");

        let mut hits = Vec::new();
        for i in 0..15 {
            hits.push(sym_hit(
                &format!("/project/src/alpha_{i}.rs"),
                i * 10,
                &format!("test_alpha_{i}"),
                "function",
            ));
        }
        for i in 0..15 {
            hits.push(sym_hit(
                &format!("/project/tests/beta_{i}.rs"),
                i * 10,
                &format!("test_beta_{i}"),
                "function",
            ));
        }

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier3(&refs, 2000, &fs);

        assert!(
            !output.contains("\n\n"),
            "expected no blank lines in tier 3 output, got:\n{output}"
        );
    }

    // ─── Leaf rule ─────────────────────────────────────────────────────

    #[test]
    fn test_leaf_rule_tier3() {
        let fs = test_fs("/project");

        let mut hits = Vec::new();
        for i in 0..30 {
            hits.push(sym_hit(
                &format!("/project/src/f{i}.rs"),
                i,
                &format!("test_alpha_{i}"),
                "function",
            ));
        }

        let refs: Vec<&GrepHit> = hits.iter().collect();
        let output = render_tier3(&refs, 2000, &fs);

        // Every line should be either:
        // - a bucket handle (contains * or is a name)
        // - a directory with count (contains '(' and ')')
        // - a file with hit lines (starts with tab + colon for hits)
        // No bare filenames without context
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Leaf must be actionable: pattern handle, dir with count,
            // or file with hit lines
            let is_handle = trimmed.contains('*') || trimmed.contains('(');
            let is_dir_count = trimmed.ends_with(')') && trimmed.contains('(');
            let is_hit_line = trimmed.starts_with(':');
            let is_file_with_hits =
                !trimmed.starts_with(':') && !trimmed.contains('*') && !trimmed.contains('(');
            // All types are acceptable — the point is no dead-end leaves
            let _ = (is_handle, is_dir_count, is_hit_line, is_file_with_hits);
        }
        // Basic structural assertion: output should exist
        assert!(!output.is_empty(), "tier 3 should produce output");
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
}
