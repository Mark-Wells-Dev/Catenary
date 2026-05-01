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
use crate::lsp::{LspClient, LspClientManager};
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
    uncovered: Vec<TrackedEntry>,
}

/// Per-file cached entries for paging.
struct CachedFile {
    display: String,
    /// Owning workspace root, or `None` for out-of-root files.
    root: Option<PathBuf>,
    /// All formatted entries, combined across all servers in
    /// server-name order.
    entries: Vec<String>,
}

/// Entry with root tracking for root-grouped output.
#[derive(Clone)]
struct TrackedEntry {
    display: String,
    root: Option<PathBuf>,
}

/// Handles `PostToolUse` hook requests: file-change notification with LSP
/// diagnostics collection and formatting.
pub struct DiagnosticsServer {
    client_manager: Arc<LspClientManager>,
    path_validator: Arc<RwLock<PathValidator>>,
    fs: Arc<FilesystemManager>,
    /// Cached full diagnostics from the last batch run, for paging.
    cache: std::sync::Mutex<Option<DiagnosticsCache>>,
}

impl DiagnosticsServer {
    /// Creates a new `DiagnosticsServer`.
    pub const fn new(
        client_manager: Arc<LspClientManager>,
        path_validator: Arc<RwLock<PathValidator>>,
        fs: Arc<FilesystemManager>,
    ) -> Self {
        Self {
            client_manager,
            path_validator,
            fs,
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
        let mut uncovered: Vec<TrackedEntry> = Vec::new();

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
                uncovered.push(TrackedEntry {
                    display: self.display_rel(&file_str),
                    root: self.fs.resolve_root(file),
                });
                continue;
            };

            let Ok(canonical) = validator.validate_read(&path) else {
                uncovered.push(TrackedEntry {
                    display: self.display_rel(&file_str),
                    root: self.fs.resolve_root(&path),
                });
                continue;
            };

            let clients = self.client_manager.diagnostic_servers(&canonical).await;
            if clients.is_empty() {
                uncovered.push(TrackedEntry {
                    display: self.display_rel(&canonical.to_string_lossy()),
                    root: self.fs.resolve_root(&canonical),
                });
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
                    root: self.fs.resolve_root(std::path::Path::new(key)),
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
                    root: self.fs.resolve_root(std::path::Path::new(key)),
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
                    root: self.fs.resolve_root(cp),
                });
            }
        }

        let cache = DiagnosticsCache {
            per_page,
            files: cached_files,
            clean: clean.clone(),
            uncovered: uncovered.clone(),
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

        let pre_detector = IdleDetector::unconditional();
        let pre_result = await_idle(&server, pre_detector, cancel.clone()).await;
        debug!(
            server = %server_name,
            "batch idle result: {pre_result:?}",
        );

        if pre_result == SettleResult::RootDied
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
            let baseline_ticks = {
                let s = Arc::clone(&server);
                tokio::task::spawn_blocking(move || {
                    s.sample_tree().map_or(0, |snap| snap.cumulative_ticks)
                })
                .await
                .unwrap_or(0)
            };

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

            let post_detector = IdleDetector::after_activity(baseline_ticks);
            let post_result = await_idle(&server, post_detector, cancel).await;
            debug!(
                server = %server_name,
                "batch post-didSave idle result: {post_result:?}",
            );

            if post_result == SettleResult::RootDied
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

            let entries = format_diagnostics_entries(
                &diagnostics,
                &fixes,
                filter,
                &server_command,
                server_version.as_deref(),
                &lang_id,
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

    /// Makes a path relative to the owning workspace root, for display.
    fn display_rel(&self, file: &str) -> String {
        let path = std::path::Path::new(file);
        self.fs.resolve_root(path).map_or_else(
            || file.to_string(),
            |root| {
                path.strip_prefix(&root).map_or_else(
                    |_| file.to_string(),
                    |rel| rel.to_string_lossy().to_string(),
                )
            },
        )
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
pub(crate) fn format_diagnostics_entries(
    diagnostics: &[Value],
    fixes: &[Vec<String>],
    filter: &dyn crate::filter::DiagnosticFilter,
    server_command: &str,
    server_version: Option<&str>,
    language_id: &str,
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
/// Groups output by workspace root with `Root:` / `OutOfRoots:` headers.
/// Appends `[cursor: ...]` at the end when more entries remain,
/// matching the pattern used by Catenary's grep and glob tools.
#[allow(clippy::too_many_lines, reason = "Root-grouped formatting pipeline")]
fn format_page(cache: &DiagnosticsCache, page: usize) -> String {
    use std::fmt::Write;

    let per_page = cache.per_page;
    let start = (page - 1) * per_page;
    let mut has_more = false;

    // Per-root collected data.
    let mut root_diags: BTreeMap<&PathBuf, String> = BTreeMap::new();
    let mut root_clean: BTreeMap<&PathBuf, Vec<&str>> = BTreeMap::new();
    let mut root_uncovered: BTreeMap<&PathBuf, Vec<&str>> = BTreeMap::new();
    let mut oor_diags = String::new();
    let mut oor_clean: Vec<&str> = Vec::new();
    let mut oor_uncovered: Vec<&str> = Vec::new();

    for cached in cache.files.values() {
        let end = cached.entries.len().min(start + per_page);
        if start >= cached.entries.len() {
            continue;
        }
        let page_entries = &cached.entries[start..end];
        if page_entries.is_empty() {
            match &cached.root {
                Some(r) => root_clean.entry(r).or_default().push(&cached.display),
                None => oor_clean.push(&cached.display),
            }
            continue;
        }

        let diags = cached
            .root
            .as_ref()
            .map_or(&mut oor_diags, |r| root_diags.entry(r).or_default());
        _ = writeln!(diags, "{}:", cached.display);
        for entry in page_entries {
            for line in entry.lines() {
                _ = writeln!(diags, "\t{line}");
            }
        }
        let remaining = cached.entries.len() - end;
        if remaining > 0 {
            has_more = true;
            _ = writeln!(diags, "\t... {remaining} more");
        }
    }

    if page == 1 {
        for entry in &cache.clean {
            match &entry.root {
                Some(r) => root_clean.entry(r).or_default().push(&entry.display),
                None => oor_clean.push(&entry.display),
            }
        }
        for entry in &cache.uncovered {
            match &entry.root {
                Some(r) => root_uncovered.entry(r).or_default().push(&entry.display),
                None => oor_uncovered.push(&entry.display),
            }
        }
    }

    // Collect all roots with any content.
    let mut all_roots: BTreeSet<&PathBuf> = BTreeSet::new();
    all_roots.extend(root_diags.keys());
    all_roots.extend(root_clean.keys());
    all_roots.extend(root_uncovered.keys());

    let mut output = String::new();

    for root in &all_roots {
        if !output.is_empty() {
            output.push('\n');
        }
        _ = writeln!(output, "Root: {}", root.display());
        if let Some(diags) = root_diags.get(root) {
            output.push_str(diags);
        }
        if let Some(clean) = root_clean.get(root)
            && !clean.is_empty()
        {
            _ = writeln!(output, "clean:");
            for f in clean {
                _ = writeln!(output, "\t{f}");
            }
        }
        if let Some(uncov) = root_uncovered.get(root)
            && !uncov.is_empty()
        {
            _ = writeln!(output, "N/A:");
            for f in uncov {
                _ = writeln!(output, "\t{f}");
            }
        }
    }

    let has_oor = !oor_diags.is_empty() || !oor_clean.is_empty() || !oor_uncovered.is_empty();
    if has_oor {
        if !output.is_empty() {
            output.push('\n');
        }
        _ = writeln!(output, "OutOfRoots:");
        if !oor_diags.is_empty() {
            output.push_str(&oor_diags);
        }
        if !oor_clean.is_empty() {
            _ = writeln!(output, "clean:");
            for f in &oor_clean {
                _ = writeln!(output, "\t{f}");
            }
        }
        if !oor_uncovered.is_empty() {
            _ = writeln!(output, "N/A:");
            for f in &oor_uncovered {
                _ = writeln!(output, "\t{f}");
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
                root: Some(PathBuf::from("/test")),
                entries,
            },
        );
        DiagnosticsCache {
            per_page,
            files,
            clean: Vec::new(),
            uncovered: Vec::new(),
        }
    }

    #[test]
    fn format_page_single_page_no_cursor() {
        let entries = vec![":1:1 [error] test: msg".to_string()];
        let cache = make_cache(entries, 50);
        let output = format_page(&cache, 1);
        assert!(output.contains("Root: /test"), "output: {output}");
        assert!(output.contains("file.rs:"), "output: {output}");
        assert!(output.contains(":1:1 [error]"), "output: {output}");
        assert!(!output.contains("[cursor:"), "output: {output}");
    }

    #[test]
    fn format_page_truncation_emits_cursor() {
        let entries: Vec<String> = (0..5)
            .map(|i| format!(":{i}:1 [warning] test: msg {i}"))
            .collect();
        let cache = make_cache(entries, 3);
        let output = format_page(&cache, 1);
        assert!(output.contains("Root: /test"), "output: {output}");
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
    fn format_page_clean_and_uncovered_on_page1_only() {
        let root = PathBuf::from("/test");
        let cache = DiagnosticsCache {
            per_page: 50,
            files: BTreeMap::new(),
            clean: vec![TrackedEntry {
                display: "clean.rs".to_string(),
                root: Some(root.clone()),
            }],
            uncovered: vec![TrackedEntry {
                display: "other.txt".to_string(),
                root: Some(root),
            }],
        };
        let page1 = format_page(&cache, 1);
        assert!(page1.contains("Root: /test"), "page1: {page1}");
        assert!(page1.contains("clean:"), "page1: {page1}");
        assert!(page1.contains("\tclean.rs"), "page1: {page1}");
        assert!(page1.contains("N/A:"), "page1: {page1}");
        assert!(page1.contains("\tother.txt"), "page1: {page1}");

        let page2 = format_page(&cache, 2);
        assert!(!page2.contains("clean"), "page2: {page2}");
        assert!(!page2.contains("N/A"), "page2: {page2}");
    }

    #[test]
    fn format_page_multi_root_grouping() {
        let mut files = BTreeMap::new();
        files.insert(
            "/alpha/src/lib.rs".to_string(),
            CachedFile {
                display: "src/lib.rs".to_string(),
                root: Some(PathBuf::from("/alpha")),
                entries: vec![":1:1 [error] test: alpha error".to_string()],
            },
        );
        files.insert(
            "/beta/src/lib.rs".to_string(),
            CachedFile {
                display: "src/lib.rs".to_string(),
                root: Some(PathBuf::from("/beta")),
                entries: vec![":5:1 [warning] test: beta warning".to_string()],
            },
        );
        let cache = DiagnosticsCache {
            per_page: 50,
            files,
            clean: vec![TrackedEntry {
                display: "src/main.rs".to_string(),
                root: Some(PathBuf::from("/alpha")),
            }],
            uncovered: Vec::new(),
        };
        let output = format_page(&cache, 1);
        // Roots appear alphabetically.
        let alpha_pos = output.find("Root: /alpha").expect("missing /alpha");
        let beta_pos = output.find("Root: /beta").expect("missing /beta");
        assert!(alpha_pos < beta_pos, "output: {output}");
        assert!(output.contains("alpha error"), "output: {output}");
        assert!(output.contains("beta warning"), "output: {output}");
        // Clean under alpha root.
        assert!(output.contains("clean:"), "output: {output}");
        assert!(output.contains("\tsrc/main.rs"), "output: {output}");
    }

    #[test]
    fn format_page_out_of_root() {
        let mut files = BTreeMap::new();
        files.insert(
            "/tmp/scratch.rs".to_string(),
            CachedFile {
                display: "/tmp/scratch.rs".to_string(),
                root: None,
                entries: vec![":3:1 [warning] test: oor warning".to_string()],
            },
        );
        let cache = DiagnosticsCache {
            per_page: 50,
            files,
            clean: Vec::new(),
            uncovered: vec![TrackedEntry {
                display: "/tmp/notes.txt".to_string(),
                root: None,
            }],
        };
        let output = format_page(&cache, 1);
        assert!(output.contains("OutOfRoots:"), "output: {output}");
        assert!(output.contains("oor warning"), "output: {output}");
        assert!(output.contains("N/A:"), "output: {output}");
        assert!(output.contains("\t/tmp/notes.txt"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
    }
}
