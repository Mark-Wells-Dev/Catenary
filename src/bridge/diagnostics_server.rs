// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, idle detection, diagnostics retrieval (push cache
//! first, pull fallback), severity filtering, noise filtering, quick-fix
//! collection, and compact formatting.

use super::filesystem_manager::FilesystemManager;
use super::path_security::PathValidator;
use crate::lsp::settle::{IdleDetector, SettleResult, await_idle};
use crate::lsp::state::ServerLifecycle;
use crate::lsp::server::LspServer;
use crate::lsp::{LspClient, LspClientManager};
use crate::symbol_index::SymbolIndex;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Per-server diagnostics result from [`DiagnosticsServer::run_server_batch`].
struct ServerDiagnostics {
    /// Formatted diagnostic entries (one per diagnostic, position order).
    entries: Vec<String>,
}

/// Cached diagnostics for paging beyond page 1.
struct DiagnosticsCache {
    per_page: usize,
    files: BTreeMap<String, CachedFile>,
    clean: Vec<TrackedEntry>,
}

/// Per-file cached entries for paging.
struct CachedFile {
    display: String,
    /// Grouping root: workspace root, or parent directory for
    /// single-file-server files outside all roots.
    root: PathBuf,
    /// All formatted entries, combined across all servers in
    /// server-name order.
    entries: Vec<String>,
}

/// Entry with root tracking for root-grouped output.
#[derive(Clone)]
struct TrackedEntry {
    display: String,
    /// Grouping root: workspace root, or parent directory for
    /// single-file-server files outside all roots.
    root: PathBuf,
}

/// Handles `PostToolUse` hook requests: file-change notification with LSP
/// diagnostics collection and formatting.
pub struct DiagnosticsServer {
    client_manager: Arc<LspClientManager>,
    path_validator: Arc<RwLock<PathValidator>>,
    fs: Arc<FilesystemManager>,
    /// Symbol index for enclosing-symbol annotation on diagnostics.
    symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    /// Cached full diagnostics from the last batch run, for paging.
    cache: std::sync::Mutex<Option<DiagnosticsCache>>,
}

impl DiagnosticsServer {
    /// Creates a new `DiagnosticsServer`.
    pub const fn new(
        client_manager: Arc<LspClientManager>,
        path_validator: Arc<RwLock<PathValidator>>,
        fs: Arc<FilesystemManager>,
        symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    ) -> Self {
        Self {
            client_manager,
            path_validator,
            fs,
            symbol_index,
            cache: std::sync::Mutex::new(None),
        }
    }

    /// Processes multiple file changes with a batched lifecycle so
    /// servers see all modified files simultaneously.
    ///
    /// Pipeline: notify file changes → resolve + canonicalize →
    /// group by server → per server (open all → settle → health
    /// probe → didSave all → settle → retrieve per file → close
    /// all) → format → `mark_current`.
    ///
    /// Cross-file diagnostics (e.g., a renamed type that breaks
    /// importers) are correct because every server sees the complete
    /// final state before producing diagnostics.
    #[allow(
        clippy::too_many_lines,
        reason = "Batch pipeline steps are sequential and cannot be split"
    )]
    #[allow(
        clippy::type_complexity,
        reason = "Server grouping map is local and self-documenting"
    )]
    pub async fn process_files_batched(&self, files: &[PathBuf], entry_id: i64) -> String {
        if files.is_empty() {
            return "[clean]\n".to_string();
        }

        // Notify servers about filesystem changes once before the batch.
        self.client_manager.notify_file_changes().await;

        // Ensure servers exist for all files before looking them up.
        // Triggers lazy spawn for files in sub-roots that haven't
        // been visited by grep/glob yet (root marker resolution).
        self.client_manager.ensure_clients_for_paths(files).await;

        // ── Phase 1: resolve + canonicalize ────────────────────────
        let mut canonical_paths: Vec<PathBuf> = Vec::new();

        // Server → list of canonical paths.
        // Keyed by server name for stable (alphabetical) iteration order.
        let mut server_groups: BTreeMap<String, (Arc<Mutex<LspClient>>, Vec<PathBuf>)> =
            BTreeMap::new();

        let validator = self.path_validator.read().await;
        for file in files {
            let file_str = file.to_string_lossy();

            // Resolve to absolute if needed (drain_all_and_clear
            // already returns absolute paths, but be defensive).
            let Ok(path) = resolve_path(&file_str) else {
                continue;
            };

            let Ok(canonical) = validator.validate_read(&path) else {
                continue;
            };

            // Files without LSP coverage are omitted from the output.
            let clients = self.client_manager.diagnostic_servers(&canonical).await;
            if clients.is_empty() {
                continue;
            }

            canonical_paths.push(canonical.clone());

            for client_mutex in &clients {
                let name = client_mutex.lock().await.server_name().to_string();
                server_groups
                    .entry(name)
                    .or_insert_with(|| (Arc::clone(client_mutex), Vec::new()))
                    .1
                    .push(canonical.clone());
            }
        }
        drop(validator);

        // ── Phase 2: per-server batch lifecycle ────────────────────
        // Collect per-file diagnostics across all servers.
        // Key: canonical path string → (display path, Vec<ServerDiagnostics>).
        let mut file_results: BTreeMap<String, (String, Vec<ServerDiagnostics>)> = BTreeMap::new();

        for (client_mutex, paths) in server_groups.values() {
            self.run_server_batch(client_mutex, paths, entry_id, &mut file_results)
                .await;
        }

        // ── Phase 3: build cache and format page 1 ──────────────
        let per_page = {
            let config = self.client_manager.config();
            config.tools.as_ref().map_or(50, |t| t.diagnostics_per_page)
        };

        let mut cached_files: BTreeMap<String, CachedFile> = BTreeMap::new();
        let mut clean: Vec<TrackedEntry> = Vec::new();

        for (key, (display, segments)) in &file_results {
            let has_any = segments.iter().any(|s| !s.entries.is_empty());
            if !has_any {
                clean.push(TrackedEntry {
                    display: display.clone(),
                    root: self.resolve_root_or_parent(std::path::Path::new(key)),
                });
                continue;
            }

            let mut all_entries: Vec<String> = Vec::new();
            for seg in segments {
                all_entries.extend(seg.entries.iter().cloned());
            }

            cached_files.insert(
                key.clone(),
                CachedFile {
                    display: display.clone(),
                    root: self.resolve_root_or_parent(std::path::Path::new(key)),
                    entries: all_entries,
                },
            );
        }

        // Files that were validated but had no server results (all
        // servers died during pipeline) — treat as clean.
        for cp in &canonical_paths {
            let key = cp.to_string_lossy().to_string();
            if !file_results.contains_key(&key) {
                clean.push(TrackedEntry {
                    display: self.display_rel(&key),
                    root: self.resolve_root_or_parent(cp),
                });
            }
        }

        let cache = DiagnosticsCache {
            per_page,
            files: cached_files,
            clean: clean.clone(),
        };

        let output = format_page(&cache, 1);

        // Store cache for subsequent pages.
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(cache);
        }

        // ── Phase 4: mark_current ─────────────────────────────────
        self.fs.mark_current(&canonical_paths);

        output
    }

    /// Runs the batched diagnostics lifecycle on a single server.
    ///
    /// Opens all files, settles, runs health probe if needed,
    /// sends didSave, settles again, retrieves diagnostics per file,
    /// and closes all files.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across pipeline for exclusive access"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "Pipeline steps are sequential and cannot be split"
    )]
    async fn run_server_batch(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        entry_id: i64,
        file_results: &mut BTreeMap<String, (String, Vec<ServerDiagnostics>)>,
    ) {
        // ── Settle before opens ───────────────────────────────────
        // Wait for the server to go idle before sending didOpen.
        // notify_file_changes() may have sent didChangeWatchedFiles
        // which triggers re-indexing — opening files while that is
        // in progress can produce transient false-positive diagnostics
        // that persist in the push cache.
        //
        // After settling, sample baseline ticks for post-open
        // activity detection (the server is confirmed idle here).
        let post_open_baseline = {
            let client = client_mutex.lock().await;
            if matches!(
                client.lifecycle(),
                ServerLifecycle::Failed | ServerLifecycle::Dead
            ) {
                return;
            }
            let server = client.server().clone();
            let server_name = client.server_name().to_string();
            drop(client);

            let detector = IdleDetector::unconditional();
            let result = await_idle(&server, detector, CancellationToken::new()).await;
            debug!(
                server = %server_name,
                "batch pre-open idle result: {result:?}",
            );
            if result == SettleResult::RootDied {
                return;
            }

            sample_baseline(&server).await
        };

        // ── Open all files ─────────────────────────────────────────
        let mut opened_uris: Vec<(PathBuf, String)> = Vec::new();

        for path in paths {
            match self
                .client_manager
                .open_document_on(path, client_mutex, Some(entry_id))
                .await
            {
                Ok(uri) => opened_uris.push((path.clone(), uri)),
                Err(e) => {
                    let name = client_mutex.lock().await.server_name().to_string();
                    warn!(
                        server = %name,
                        path = %path.display(),
                        "batch open failed, skipping file: {e}",
                    );
                }
            }
        }

        if opened_uris.is_empty() {
            return;
        }

        // ── Settle after all opens ─────────────────────────────────
        let client = client_mutex.lock().await;

        if matches!(
            client.lifecycle(),
            ServerLifecycle::Failed | ServerLifecycle::Dead
        ) {
            // Close whatever we opened and bail.
            drop(client);
            self.close_all(client_mutex, &opened_uris).await;
            return;
        }

        let server = client.server().clone();
        let server_name = client.server_name().to_string();
        let cancel = CancellationToken::new();

        if !settle_after(&server, post_open_baseline, cancel.clone(), &server_name, "post-open")
            .await
            || matches!(
                client.lifecycle(),
                ServerLifecycle::Failed | ServerLifecycle::Dead
            )
        {
            drop(client);
            self.close_all(client_mutex, &opened_uris).await;
            return;
        }

        // ── Health probe ───────────────────────────────────────────
        if client.lifecycle() == ServerLifecycle::Probing
            && !client.run_health_probe(&opened_uris[0].1).await
        {
            drop(client);
            self.close_all(client_mutex, &opened_uris).await;
            return;
        }

        // ── didSave all ────────────────────────────────────────────
        if client.wants_did_save() {
            let baseline = sample_baseline(&server).await;

            let mut save_failed = false;
            for (_, uri) in &opened_uris {
                if let Err(e) = client.did_save(uri).await {
                    warn!(
                        server = %server_name,
                        "batch didSave failed: {e}",
                    );
                    save_failed = true;
                    break;
                }
            }

            if save_failed {
                drop(client);
                self.close_all(client_mutex, &opened_uris).await;
                return;
            }

            if !settle_after(&server, baseline, cancel, &server_name, "post-didSave").await
                || matches!(
                    client.lifecycle(),
                    ServerLifecycle::Failed | ServerLifecycle::Dead
                )
            {
                drop(client);
                self.close_all(client_mutex, &opened_uris).await;
                return;
            }
        }

        // ── Retrieve diagnostics per file ──────────────────────────
        let server_command = client.server_command().to_string();
        let server_version = client.server_version().map(str::to_string);
        let lang_id = client.language().to_string();
        let has_code_actions = client
            .capabilities()
            .get("codeActionProvider")
            .is_some_and(|v| !v.is_null());

        for (path, uri) in &opened_uris {
            let diagnostics = {
                let cached = client.get_diagnostics(uri);
                if !cached.is_empty() {
                    cached
                } else if client.supports_pull_diagnostics() {
                    match client.pull_diagnostics(uri).await {
                        Ok(diags) => diags,
                        Err(e) => {
                            client.server().downgrade_pull_diagnostics();
                            debug!("pull diagnostics failed, downgraded: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                }
            };

            // Apply per-server min_severity filter before quick-fix
            // collection so we don't waste code-action requests on
            // diagnostics that will be dropped.
            let min_severity = {
                let config = self.client_manager.config();
                config
                    .server
                    .get(&server_name)
                    .and_then(|sd| sd.min_severity.as_deref())
                    .and_then(crate::filter::parse_severity)
            };

            let diagnostics = if let Some(threshold) = min_severity {
                diagnostics
                    .into_iter()
                    .filter(|d| {
                        crate::lsp::extract::diagnostic_severity(d)
                            .is_none_or(|sev| crate::filter::severity_passes(sev, threshold))
                    })
                    .collect()
            } else {
                diagnostics
            };

            let fixes = if !diagnostics.is_empty() && has_code_actions {
                collect_quick_fixes(&client, uri, &diagnostics).await
            } else {
                Vec::new()
            };

            let filter = crate::filter::get_filter(&server_command);

            // Populate symbol index if needed — the file is already open
            // on this server, so documentSymbol is a single request.
            if let Some(ref idx_arc) = self.symbol_index {
                let needs = idx_arc.lock().is_ok_and(|idx| !idx.has_symbols_for(path));
                if needs
                    && client.server().supports_document_symbols()
                    && let Ok(response) = client.document_symbols(uri).await
                    && let Ok(idx) = idx_arc.lock()
                {
                    let _ = idx.populate_from_document_symbols(path, &response);
                }
            }

            let enclosing_symbols =
                resolve_enclosing_symbols(self.symbol_index.as_ref(), path, &diagnostics);

            let entries = format_diagnostics_entries(
                &diagnostics,
                &fixes,
                filter,
                &server_command,
                server_version.as_deref(),
                &lang_id,
                &enclosing_symbols,
            );

            let key = path.to_string_lossy().to_string();
            let display = self.display_rel(&key);
            file_results
                .entry(key)
                .or_insert_with(|| (display, Vec::new()))
                .1
                .push(ServerDiagnostics { entries });
        }

        // ── Close all ──────────────────────────────────────────────
        drop(client);
        self.close_all(client_mutex, &opened_uris).await;
    }

    /// Closes all opened documents on a server and clears `parent_id`.
    async fn close_all(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        opened_uris: &[(PathBuf, String)],
    ) {
        let mut client = client_mutex.lock().await;
        for (_, uri) in opened_uris {
            client.close_tracked_document(uri).await;
        }
        client.set_parent_id(None);
    }

    /// Makes a path relative to its grouping root, for display.
    ///
    /// Files within a workspace root are shown relative to that root.
    /// Files outside all roots (single-file servers) are shown as the
    /// bare filename — the parent directory becomes the section header.
    fn display_rel(&self, file: &str) -> String {
        let path = std::path::Path::new(file);
        self.fs.resolve_root(path).map_or_else(
            || {
                path.file_name().map_or_else(
                    || file.to_string(),
                    |name| name.to_string_lossy().to_string(),
                )
            },
            |root| {
                path.strip_prefix(&root).map_or_else(
                    |_| file.to_string(),
                    |rel| rel.to_string_lossy().to_string(),
                )
            },
        )
    }

    /// Returns the grouping root for a file path.
    ///
    /// Workspace root when available, otherwise the file's parent
    /// directory (for single-file-server files outside all roots).
    fn resolve_root_or_parent(&self, path: &std::path::Path) -> PathBuf {
        self.fs.resolve_root(path).unwrap_or_else(|| {
            path.parent()
                .map_or_else(|| PathBuf::from("/"), PathBuf::from)
        })
    }

    /// Returns a formatted page of cached diagnostics.
    ///
    /// Page 1 is produced by [`Self::process_files_batched`]. Pages 2+
    /// are served from the cache built during that call. Returns `None`
    /// if the cache is empty (no prior `done_editing` call).
    /// Serves a page of cached diagnostics identified by an opaque cursor.
    ///
    /// Returns `None` if the cursor is invalid or the cache is empty.
    pub fn get_cursor(&self, token: &str) -> Option<String> {
        let page = decode_cursor(token)?;
        let guard = self.cache.lock().ok()?;
        let cache = guard.as_ref()?;
        let result = format_page(cache, page);
        drop(guard);
        Some(result)
    }

    /// Clears the diagnostics page cache.
    ///
    /// Called on `start_editing` so that stale pages from a previous
    /// batch cannot be served during the new editing session.
    pub fn clear_cache(&self) {
        if let Ok(mut guard) = self.cache.lock() {
            *guard = None;
        }
    }
}

/// Samples cumulative ticks for use as an [`IdleDetector::after_activity`]
/// baseline. Returns 0 if the tree monitor is unavailable.
async fn sample_baseline(server: &Arc<LspServer>) -> u64 {
    let s = Arc::clone(server);
    tokio::task::spawn_blocking(move || {
        s.sample_tree().map_or(0, |snap| snap.cumulative_ticks)
    })
    .await
    .unwrap_or(0)
}

/// Settles after a stimulus using [`IdleDetector::after_activity`].
///
/// Returns `true` if the server settled normally, `false` if the root
/// process died (caller should close files and bail).
async fn settle_after(
    server: &Arc<LspServer>,
    baseline: u64,
    cancel: CancellationToken,
    server_name: &str,
    label: &str,
) -> bool {
    let detector = IdleDetector::after_activity(baseline);
    let result = await_idle(server, detector, cancel).await;
    debug!(
        server = %server_name,
        "batch {label} idle result: {result:?}",
    );
    result != SettleResult::RootDied
}

/// Resolves a file path to an absolute path.
pub(crate) fn resolve_path(file: &str) -> Result<PathBuf> {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?;
        Ok(cwd.join(path))
    }
}

/// Collects quick-fix titles for each diagnostic from the LSP server.
///
/// Returns a `Vec` parallel to `diagnostics` — each entry contains the
/// titles of quick-fix code actions for that diagnostic. Diagnostics
/// without fixes get an empty vec.
///
/// Requests are dispatched concurrently via `futures::future::join_all`
/// to avoid sequential per-diagnostic latency (25-30 diagnostics is
/// common in real-world files).
async fn collect_quick_fixes(
    client: &LspClient,
    uri: &str,
    diagnostics: &[Value],
) -> Vec<Vec<String>> {
    let futures: Vec<_> = diagnostics
        .iter()
        .map(|diag| async move {
            let Some(range) = crate::lsp::extract::diagnostic_range(diag) else {
                return Vec::new();
            };
            let diag_slice = [diag.clone()];
            client
                .code_action(
                    uri,
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                    &diag_slice,
                )
                .await
                .map_or_else(
                    |_| Vec::new(),
                    |result| {
                        result
                            .as_array()
                            .map(|actions| {
                                actions
                                    .iter()
                                    .filter_map(|a| {
                                        if a.get("kind").and_then(Value::as_str) == Some("quickfix")
                                        {
                                            a.get("title")
                                                .and_then(Value::as_str)
                                                .map(str::to_string)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    },
                )
        })
        .collect();

    futures::future::join_all(futures).await
}

/// Resolves the innermost enclosing symbol name for each diagnostic.
///
/// Returns a vec parallel to `diagnostics`. Each entry is `Some(name)` when
/// a symbol encloses the diagnostic's start line, `None` otherwise.
/// Returns an empty vec when the symbol index is unavailable.
fn resolve_enclosing_symbols(
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    file_path: &std::path::Path,
    diagnostics: &[Value],
) -> Vec<Option<String>> {
    let Some(index_arc) = symbol_index else {
        return Vec::new();
    };
    let Ok(index) = index_arc.lock() else {
        return Vec::new();
    };
    if !index.has_symbols_for(file_path) {
        return Vec::new();
    }
    diagnostics
        .iter()
        .map(|d| {
            let line_0 = crate::lsp::extract::diagnostic_range(d).map(|r| r.start.line)?;
            index
                .find_enclosing(file_path, line_0)
                .ok()
                .flatten()
                .map(|sym| sym.name)
        })
        .collect()
}

/// Formats diagnostics as individual entry strings.
///
/// Each entry contains the line/column, severity, message, and optional
/// quick-fix titles. Returns one string per diagnostic (may span multiple
/// lines when fixes are present). Diagnostics whose noise-filtered message
/// is empty are dropped.
///
/// `fixes` is parallel to `diagnostics` — each entry contains the titles of
/// quick-fix code actions for that diagnostic. Pass an empty slice when no
/// fixes were collected.
///
/// `enclosing_symbols` is parallel to `diagnostics` — each entry is the
/// name of the innermost enclosing symbol (from `SymbolIndex`), or `None`
/// when no symbol encloses the diagnostic. Pass an empty slice when the
/// symbol index is unavailable.
pub(crate) fn format_diagnostics_entries(
    diagnostics: &[Value],
    fixes: &[Vec<String>],
    filter: &dyn crate::filter::DiagnosticFilter,
    server_command: &str,
    server_version: Option<&str>,
    language_id: &str,
    enclosing_symbols: &[Option<String>],
) -> Vec<String> {
    diagnostics
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let severity = match crate::lsp::extract::diagnostic_severity(d) {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                Some(4) => "hint",
                _ => "unknown",
            };
            let (line, col) = crate::lsp::extract::diagnostic_range(d)
                .map_or((0, 0), |r| (r.start.line + 1, r.start.character + 1));
            let source = d.get("source").and_then(Value::as_str);
            let source_str = source.unwrap_or("");
            let code_value = d.get("code");
            let code = code_value
                .map(|c| {
                    c.as_i64().map_or_else(
                        || c.as_str().map_or_else(|| c.to_string(), str::to_string),
                        |n| n.to_string(),
                    )
                })
                .unwrap_or_default();

            let diag_code = code_value.map(crate::filter::DiagnosticCode::from_value);
            let message = filter.filter_message(
                server_command,
                server_version,
                source,
                diag_code.as_ref(),
                crate::lsp::extract::diagnostic_severity(d)
                    .unwrap_or(crate::filter::SEVERITY_WARNING),
                language_id,
                crate::lsp::extract::diagnostic_message(d).unwrap_or(""),
            );

            // Empty message means the filter wants to drop this diagnostic
            if message.is_empty() {
                return None;
            }

            let mut result = if code.is_empty() {
                format!(":{line}:{col} [{severity}] {source_str}: {message}")
            } else {
                format!(":{line}:{col} [{severity}] {source_str}({code}): {message}")
            };

            // Append enclosing symbol context
            if let Some(Some(name)) = enclosing_symbols.get(i) {
                use std::fmt::Write;
                let _ = write!(result, " (in {name})");
            }

            // Append indented fix lines
            if let Some(fix_titles) = fixes.get(i) {
                for title in fix_titles {
                    use std::fmt::Write;
                    let _ = write!(result, "\n\tfix: {title}");
                }
            }

            Some(result)
        })
        .collect()
}

// ─── Cursor-based paging ──────────────────────────────────────────────

/// Encodes an opaque cursor token from a 1-based page number.
fn encode_cursor(page: usize) -> String {
    format!("d{page}")
}

/// Decodes an opaque cursor token to a 1-based page number.
fn decode_cursor(token: &str) -> Option<usize> {
    token.strip_prefix('d')?.parse().ok()
}

/// Formats a page of diagnostics from the cache.
///
/// Output starts with `[LSP available]`, followed by bare root-path
/// section headers. Files without LSP coverage are omitted. Clean
/// files are listed inline with `[clean]`. Appends `[cursor: ...]`
/// when more entries remain.
///
/// When a root contains a single file, the root and filename are
/// collapsed into one path (e.g. `/tmp/scratch.sh`). Multi-file
/// roots get a directory header with indented file entries beneath.
fn format_page(cache: &DiagnosticsCache, page: usize) -> String {
    use std::fmt::Write;

    let per_page = cache.per_page;
    let start = (page - 1) * per_page;
    let mut has_more = false;

    // Per-root: diagnostic file entries collected without indentation
    // so the render pass can choose single-file vs multi-file layout.
    let mut root_diag_files: BTreeMap<&PathBuf, Vec<(&str, String)>> = BTreeMap::new();
    let mut root_clean: BTreeMap<&PathBuf, Vec<&str>> = BTreeMap::new();

    for cached in cache.files.values() {
        let end = cached.entries.len().min(start + per_page);
        if start >= cached.entries.len() {
            continue;
        }
        let page_entries = &cached.entries[start..end];
        if page_entries.is_empty() {
            root_clean
                .entry(&cached.root)
                .or_default()
                .push(&cached.display);
            continue;
        }

        let mut content = String::new();
        for entry in page_entries {
            for line in entry.lines() {
                _ = writeln!(content, "{line}");
            }
        }
        let remaining = cached.entries.len() - end;
        if remaining > 0 {
            has_more = true;
            _ = writeln!(content, "... {remaining} more");
        }

        root_diag_files
            .entry(&cached.root)
            .or_default()
            .push((&cached.display, content));
    }

    if page == 1 {
        for entry in &cache.clean {
            root_clean
                .entry(&entry.root)
                .or_default()
                .push(&entry.display);
        }
    }

    // Collect all roots with any content.
    let mut all_roots: BTreeSet<&PathBuf> = BTreeSet::new();
    all_roots.extend(root_diag_files.keys());
    all_roots.extend(root_clean.keys());

    let mut output = String::new();

    if page == 1 {
        _ = writeln!(output, "[LSP available]");
    }

    for root in &all_roots {
        let diag_count = root_diag_files.get(root).map_or(0, Vec::len);
        let clean_count = root_clean.get(root).map_or(0, Vec::len);
        let total = diag_count + clean_count;
        let collapsed = total == 1;

        output.push('\n');

        if collapsed {
            // Single file: merge root and filename into one path.
            if let Some(files) = root_diag_files.get(root) {
                for (display, content) in files {
                    _ = writeln!(output, "{}:", root.join(display).display());
                    for line in content.lines() {
                        _ = writeln!(output, "\t{line}");
                    }
                }
            }
            if let Some(clean) = root_clean.get(root) {
                for f in clean {
                    _ = writeln!(output, "{}", root.join(f).display());
                    _ = writeln!(output, "\t[clean]");
                }
            }
        } else {
            // Multiple files: directory header with indented entries.
            _ = writeln!(output, "{}", root.display());
            if let Some(files) = root_diag_files.get(root) {
                for (display, content) in files {
                    _ = writeln!(output, "\t{display}:");
                    for line in content.lines() {
                        _ = writeln!(output, "\t\t{line}");
                    }
                }
            }
            if let Some(clean) = root_clean.get(root) {
                for f in clean {
                    _ = writeln!(output, "\t{f}");
                    _ = writeln!(output, "\t\t[clean]");
                }
            }
        }
    }

    if has_more {
        _ = writeln!(output, "[cursor: {}]", encode_cursor(page + 1));
    }

    if output.is_empty() && page > 1 {
        output = "no more diagnostics\n".to_string();
    }

    output
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── cursor encode/decode tests ──────────────────────────────

    #[test]
    fn cursor_round_trip() {
        assert_eq!(decode_cursor(&encode_cursor(2)), Some(2));
        assert_eq!(decode_cursor(&encode_cursor(100)), Some(100));
    }

    #[test]
    fn cursor_decode_invalid() {
        assert_eq!(decode_cursor(""), None);
        assert_eq!(decode_cursor("g5"), None); // glob cursor, not diag
        assert_eq!(decode_cursor("abc"), None);
    }

    // ── format_page tests ─────────────────────────────────────────

    fn make_cache(entries: Vec<String>, per_page: usize) -> DiagnosticsCache {
        let mut files = BTreeMap::new();
        files.insert(
            "/test/file.rs".to_string(),
            CachedFile {
                display: "file.rs".to_string(),
                root: PathBuf::from("/test"),
                entries,
            },
        );
        DiagnosticsCache {
            per_page,
            files,
            clean: Vec::new(),
        }
    }

    #[test]
    fn format_page_single_page_no_cursor() {
        let entries = vec![":1:1 [error] test: msg".to_string()];
        let cache = make_cache(entries, 50);
        let output = format_page(&cache, 1);
        assert!(output.starts_with("[LSP available]\n"), "output: {output}");
        // Single file under root → collapsed path.
        assert!(output.contains("/test/file.rs:"), "output: {output}");
        assert!(output.contains("\t:1:1 [error]"), "output: {output}");
        assert!(!output.contains("[cursor:"), "output: {output}");
    }

    #[test]
    fn format_page_truncation_emits_cursor() {
        let entries: Vec<String> = (0..5)
            .map(|i| format!(":{i}:1 [warning] test: msg {i}"))
            .collect();
        let cache = make_cache(entries, 3);
        let output = format_page(&cache, 1);
        assert!(output.starts_with("[LSP available]\n"), "output: {output}");
        // Single file → collapsed path.
        assert!(output.contains("/test/file.rs:"), "output: {output}");
        assert!(output.contains("2 more"), "output: {output}");
        assert!(output.contains("[cursor: d2]"), "output: {output}");
        assert!(!output.contains("msg 3"), "output: {output}");
    }

    #[test]
    fn format_page_second_page_no_cursor() {
        let entries: Vec<String> = (0..5)
            .map(|i| format!(":{i}:1 [warning] test: msg {i}"))
            .collect();
        let cache = make_cache(entries, 3);
        let output = format_page(&cache, 2);
        assert!(output.contains("msg 3"), "output: {output}");
        assert!(output.contains("msg 4"), "output: {output}");
        assert!(!output.contains("msg 0"), "output: {output}");
        assert!(!output.contains("[cursor:"), "output: {output}");
    }

    #[test]
    fn format_page_beyond_last() {
        let entries = vec![":1:1 [error] test: msg".to_string()];
        let cache = make_cache(entries, 50);
        let output = format_page(&cache, 2);
        assert_eq!(output, "no more diagnostics\n");
    }

    #[test]
    fn format_page_clean_on_page1_only() {
        let cache = DiagnosticsCache {
            per_page: 50,
            files: BTreeMap::new(),
            clean: vec![TrackedEntry {
                display: "clean.rs".to_string(),
                root: PathBuf::from("/test"),
            }],
        };
        let page1 = format_page(&cache, 1);
        assert!(page1.starts_with("[LSP available]\n"), "page1: {page1}");
        // Single file → collapsed path with [clean].
        assert!(page1.contains("/test/clean.rs\n"), "page1: {page1}");
        assert!(page1.contains("\t[clean]"), "page1: {page1}");
        assert!(!page1.contains("N/A:"), "page1: {page1}");

        let page2 = format_page(&cache, 2);
        assert!(!page2.contains("clean"), "page2: {page2}");
    }

    #[test]
    fn format_page_multi_root_grouping() {
        let mut files = BTreeMap::new();
        files.insert(
            "/alpha/src/lib.rs".to_string(),
            CachedFile {
                display: "src/lib.rs".to_string(),
                root: PathBuf::from("/alpha"),
                entries: vec![":1:1 [error] test: alpha error".to_string()],
            },
        );
        files.insert(
            "/beta/src/lib.rs".to_string(),
            CachedFile {
                display: "src/lib.rs".to_string(),
                root: PathBuf::from("/beta"),
                entries: vec![":5:1 [warning] test: beta warning".to_string()],
            },
        );
        let cache = DiagnosticsCache {
            per_page: 50,
            files,
            clean: vec![TrackedEntry {
                display: "src/main.rs".to_string(),
                root: PathBuf::from("/alpha"),
            }],
        };
        let output = format_page(&cache, 1);
        // /alpha has 2 files (diag + clean) → expanded with directory header.
        let alpha_pos = output.find("\n/alpha\n").expect("missing /alpha header");
        assert!(output.contains("\tsrc/lib.rs:"), "output: {output}");
        assert!(output.contains("\t\t:1:1 [error]"), "output: {output}");
        assert!(output.contains("\tsrc/main.rs\n"), "output: {output}");
        assert!(output.contains("\t\t[clean]"), "output: {output}");
        // /beta has 1 file → collapsed into single path.
        let beta_pos = output
            .find("\n/beta/src/lib.rs:")
            .expect("missing /beta collapsed path");
        assert!(alpha_pos < beta_pos, "output: {output}");
        assert!(output.contains("beta warning"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
    }

    #[test]
    fn format_page_single_file_server() {
        let mut files = BTreeMap::new();
        files.insert(
            "/tmp/scratch.sh".to_string(),
            CachedFile {
                display: "scratch.sh".to_string(),
                root: PathBuf::from("/tmp"),
                entries: vec![":3:1 [warning] test: standalone warning".to_string()],
            },
        );
        let cache = DiagnosticsCache {
            per_page: 50,
            files,
            clean: Vec::new(),
        };
        let output = format_page(&cache, 1);
        assert!(output.starts_with("[LSP available]\n"), "output: {output}");
        // Single file → collapsed path.
        assert!(output.contains("/tmp/scratch.sh:"), "output: {output}");
        assert!(output.contains("\t:3:1 [warning]"), "output: {output}");
        assert!(output.contains("standalone warning"), "output: {output}");
        assert!(!output.contains("OutOfRoots:"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
        assert!(!output.contains("N/A:"), "output: {output}");
    }

    #[test]
    fn diagnostics_lsp_available_header() {
        let entries = vec![":1:1 [error] test: msg".to_string()];
        let cache = make_cache(entries, 50);
        let output = format_page(&cache, 1);
        // First line is the status header.
        assert!(output.starts_with("[LSP available]\n"), "output: {output}");
        // Bare path, no prefix.
        assert!(output.contains("/test/file.rs:"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
        // Page 2 does not repeat the header.
        let page2 = format_page(&cache, 2);
        assert!(!page2.contains("[LSP available]"), "page2: {page2}");
    }

    #[test]
    fn diagnostics_omit_uncovered() {
        // Cache with only clean files, no uncovered field at all.
        let cache = DiagnosticsCache {
            per_page: 50,
            files: BTreeMap::new(),
            clean: vec![TrackedEntry {
                display: "lib.rs".to_string(),
                root: PathBuf::from("/project"),
            }],
        };
        let output = format_page(&cache, 1);
        // No N/A or OutOfRoots sections.
        assert!(!output.contains("N/A"), "output: {output}");
        assert!(!output.contains("OutOfRoots"), "output: {output}");
        // Single file → collapsed path with [clean].
        assert!(output.contains("/project/lib.rs\n"), "output: {output}");
        assert!(output.contains("\t[clean]"), "output: {output}");
    }

    // ── enclosing symbol tests ────────────────────────────────────

    fn make_diag(line: u32, col: u32, severity: u8, msg: &str) -> Value {
        serde_json::json!({
            "range": {
                "start": { "line": line, "character": col },
                "end": { "line": line, "character": col + 1 }
            },
            "severity": severity,
            "source": "test",
            "message": msg
        })
    }

    fn make_symbol_index(entries: &[(&str, &str, &str, u32, u32)]) -> SymbolIndex {
        let idx = SymbolIndex::new().expect("symbol index creation");
        let path = Path::new("/test/file.rs");
        let symbols: Vec<serde_json::Value> = entries
            .iter()
            .map(|(name, kind_str, _scope, start, end)| {
                let kind_num = match *kind_str {
                    "function" => 12,
                    "method" => 6,
                    "struct" => 23,
                    "module" => 2,
                    _ => 0,
                };
                serde_json::json!({
                    "name": name,
                    "kind": kind_num,
                    "range": {
                        "start": { "line": start, "character": 0 },
                        "end": { "line": end, "character": 0 }
                    },
                    "selectionRange": {
                        "start": { "line": start, "character": 0 },
                        "end": { "line": start, "character": name.len() }
                    }
                })
            })
            .collect();
        let arr = serde_json::Value::Array(symbols);
        idx.populate_from_document_symbols(path, &arr)
            .expect("populate symbols");
        idx
    }

    #[test]
    fn diagnostic_with_enclosing_symbol() {
        let diags = vec![make_diag(15, 5, 2, "unused variable")];
        let filter = crate::filter::get_filter("");
        let symbols = vec![Some("my_function".to_string())];
        let entries =
            format_diagnostics_entries(&diags, &[], filter, "test", None, "rust", &symbols);
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].ends_with("(in my_function)"),
            "entry: {}",
            entries[0]
        );
    }

    #[test]
    fn diagnostic_nested_symbol() {
        // Outer: struct at lines 0-100, inner: method at lines 10-20
        let idx = SymbolIndex::new().expect("symbol index creation");
        let path = Path::new("/test/file.rs");
        let symbols = serde_json::json!([
            {
                "name": "MyStruct",
                "kind": 23,
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 100, "character": 0 }
                },
                "selectionRange": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 8 }
                },
                "children": [
                    {
                        "name": "my_method",
                        "kind": 6,
                        "range": {
                            "start": { "line": 10, "character": 0 },
                            "end": { "line": 20, "character": 0 }
                        },
                        "selectionRange": {
                            "start": { "line": 10, "character": 0 },
                            "end": { "line": 10, "character": 9 }
                        }
                    }
                ]
            }
        ]);
        idx.populate_from_document_symbols(path, &symbols)
            .expect("populate");
        let index = Some(Arc::new(std::sync::Mutex::new(idx)));

        let diags = vec![make_diag(15, 0, 1, "type mismatch")];
        let resolved = resolve_enclosing_symbols(index.as_ref(), path, &diags);
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].as_deref(),
            Some("my_method"),
            "should pick innermost symbol, not MyStruct"
        );
    }

    #[test]
    fn diagnostic_no_symbol_index() {
        let diags = vec![make_diag(5, 0, 2, "warning msg")];
        let resolved = resolve_enclosing_symbols(None, Path::new("/test/file.rs"), &diags);
        assert!(resolved.is_empty());
    }

    #[test]
    fn diagnostic_file_scope() {
        // Symbol at lines 10-20, diagnostic at line 0 (outside any symbol)
        let idx = make_symbol_index(&[("some_fn", "function", "", 10, 20)]);
        let index = Some(Arc::new(std::sync::Mutex::new(idx)));
        let path = Path::new("/test/file.rs");

        let diags = vec![make_diag(0, 0, 2, "file-level warning")];
        let resolved = resolve_enclosing_symbols(index.as_ref(), path, &diags);
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0], None,
            "file-scope diagnostic should have no symbol"
        );
    }

    #[test]
    fn diagnostic_format_unchanged_without_symbols() {
        let diags = vec![make_diag(10, 5, 1, "some error")];
        let filter = crate::filter::get_filter("");

        let with_empty = format_diagnostics_entries(&diags, &[], filter, "test", None, "rust", &[]);
        let with_none =
            format_diagnostics_entries(&diags, &[], filter, "test", None, "rust", &[None]);
        assert_eq!(
            with_empty, with_none,
            "empty slice and None should produce same output"
        );
        assert!(
            !with_empty[0].contains("(in "),
            "no symbol suffix: {}",
            with_empty[0]
        );
    }
}
