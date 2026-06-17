// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, idle detection, diagnostics retrieval (push cache
//! first, pull fallback), severity filtering, noise filtering, quick-fix
//! collection, and compact formatting.

use super::filesystem_manager::{FilesystemManager, mtime_nanos};
use super::path_security::PathValidator;
use crate::lsp::server::LspServer;
use crate::lsp::settle::{IdleDetector, SettleResult, await_idle};
use crate::lsp::state::ServerLifecycle;
use crate::lsp::{LspClient, LspClientManager, WalkBreadth};
use crate::symbol_index::SymbolIndex;
use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// A single rendered diagnostic together with its LSP severity.
///
/// The severity drives two policies that operate after rendering: the
/// errors-before-warnings preview budget, and the clean/dirty exit-code
/// threshold ([`ToolsConfig::dirty_severity`](crate::config::ToolsConfig::dirty_severity)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiagEntry {
    /// LSP severity (1=Error … 4=Hint, 5=unknown). Lower is more severe, so
    /// sorting ascending puts errors first.
    severity: u8,
    /// Rendered entry text — position, severity label, message, and any
    /// indented quick-fix lines. May span multiple lines.
    text: String,
}

/// Outcome of a `catenary diagnostics` run: the budgeted preview text plus the
/// clean/dirty status the CLI maps to its exit code.
pub struct DiagnosticsOutcome {
    /// Preview text for stdout. Includes the `… N more — full report at <path>`
    /// pointer line when the full set spilled to the overflow file.
    pub output: String,
    /// `true` when at least one diagnostic met the dirty severity threshold
    /// (exit code 1); `false` is clean (exit code 0).
    pub dirty: bool,
    /// Number of error-severity (LSP severity 1) diagnostics across the batch.
    /// Counts the complete set, not the budgeted preview — feeds the session
    /// board's `last_action` summary (observability ticket 05).
    pub errors: usize,
    /// Number of warning-severity (LSP severity 2) diagnostics across the batch.
    pub warnings: usize,
}

/// Per-server diagnostics result from [`DiagnosticsServer::run_server_batch`].
struct ServerDiagnostics {
    /// Formatted diagnostic entries (one per diagnostic, position order).
    entries: Vec<DiagEntry>,
}

/// File with diagnostics for root-grouped output.
struct DiagnosticFile {
    display: String,
    /// Grouping root: workspace root, or parent directory for
    /// single-file-server files outside all roots.
    root: PathBuf,
    /// All formatted entries, combined across all servers in
    /// server-name order.
    entries: Vec<DiagEntry>,
}

/// Clean file entry for root-grouped output.
struct CleanEntry {
    display: String,
    /// Grouping root: workspace root, or parent directory for
    /// single-file-server files outside all roots.
    root: PathBuf,
}

/// File without LSP server coverage.
struct UncoveredEntry {
    display: String,
    /// Grouping root: workspace root, or parent directory for
    /// files outside all roots.
    root: PathBuf,
}

/// Classification outcome for a single file in the batch pipeline.
///
/// Makes the three-way decision explicit: each category is a distinct
/// variant rather than an implicit negated-boolean branch across
/// separate loops.
#[derive(Debug, PartialEq, Eq)]
enum FileOutcome {
    /// At least one server returned diagnostic entries.
    HasDiagnostics(Vec<DiagEntry>),
    /// All servers returned empty diagnostics — file is clean.
    Clean,
    /// File was validated but absent from server results (server died
    /// during the pipeline before producing results).
    NoResults,
}

/// Handles `PostToolUse` hook requests: file-change notification with LSP
/// diagnostics collection and formatting.
pub struct DiagnosticsServer {
    client_manager: Arc<LspClientManager>,
    path_validator: Arc<RwLock<PathValidator>>,
    fs: Arc<FilesystemManager>,
    /// Symbol index for enclosing-symbol annotation on diagnostics.
    symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
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
        }
    }

    /// Processes multiple file changes with a batched lifecycle so
    /// servers see all modified files simultaneously.
    ///
    /// Pipeline: resolve + canonicalize → group by server → per
    /// server (open all → settle → health probe → didSave all →
    /// settle → retrieve per file → close all) → format → bump
    /// generations.
    ///
    /// Cross-file diagnostics (e.g., a renamed type that breaks
    /// importers) are correct because every server sees the complete
    /// final state before producing diagnostics.
    #[allow(
        clippy::type_complexity,
        reason = "Server grouping map is local and self-documenting"
    )]
    pub async fn process_files_batched(
        &self,
        files: &[PathBuf],
        parent_id: Option<&str>,
        session_id: &str,
    ) -> DiagnosticsOutcome {
        if files.is_empty() {
            return DiagnosticsOutcome {
                output: String::new(),
                dirty: false,
                errors: 0,
                warnings: 0,
            };
        }

        // Ensure servers exist for all files before looking them up.
        // Triggers lazy spawn for files in sub-roots that haven't
        // been visited by grep/glob yet (root marker resolution).
        self.client_manager.ensure_clients_for_paths(files).await;

        // ── Phase 1: resolve + canonicalize ────────────────────────
        let mut canonical_paths: Vec<PathBuf> = Vec::new();
        let mut uncovered: Vec<UncoveredEntry> = Vec::new();

        // Server → list of canonical paths.
        // Keyed by server name for stable (alphabetical) iteration order.
        let mut server_groups: BTreeMap<String, (Arc<Mutex<LspClient>>, Vec<PathBuf>)> =
            BTreeMap::new();

        let validator = self.path_validator.read().await;
        for file in files {
            let file_str = file.to_string_lossy();

            // Resolve to absolute if needed (the editing-manager drain
            // already returns absolute paths, but be defensive).
            let Ok(path) = resolve_path(&file_str) else {
                continue;
            };

            let Ok(canonical) = validator.validate_read(&path) else {
                continue;
            };

            let clients = self.client_manager.diagnostic_servers(&canonical).await;
            if clients.is_empty() {
                let display = self.display_rel(&canonical.to_string_lossy());
                let root = self.resolve_root_or_parent(&canonical);
                uncovered.push(UncoveredEntry { display, root });
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

        // ── Phase 1b: route the changed-set nudge (WS31 Consumer A) ──
        // Pull diagnostics read the server's index, so an external change the
        // server never saw (a `git checkout` between edits) yields stale
        // diagnostics. Under the walk-breadth gate (ticket 04), `diagnostics` is
        // always a `Full` walk for a covered root: a dedicated stat-walk of each
        // affected root's registered-glob set diffs against the per-root
        // baseline, the delta is routed per server before the batch, AND
        // deletions are reaped (a baseline entry the full walk did not visit ⇒
        // `Deleted`). A root with no covering server is `WalkBreadth::None` and
        // is skipped (no stat-walk, no nudge). The edited-set rides document-sync
        // (didOpen/didSave), so it is excluded from the emission — but its mtime
        // is still recorded in the baseline (so a later walk won't re-flag it).
        {
            let roots: std::collections::BTreeSet<PathBuf> = canonical_paths
                .iter()
                .filter_map(|p| self.fs.resolve_root(p))
                .collect();
            for root in &roots {
                let breadth = if self.client_manager.has_covering_watchers(root).await {
                    WalkBreadth::Full
                } else {
                    WalkBreadth::None
                };
                if !breadth.runs_engine() {
                    continue;
                }
                // Edited paths (relative to this root) to exclude from emission.
                let exclude: HashSet<PathBuf> = canonical_paths
                    .iter()
                    .filter_map(|p| p.strip_prefix(root).ok().map(std::path::Path::to_path_buf))
                    .collect();
                let observed = stat_walk(root);
                self.client_manager
                    .nudge_changed_set(root, &observed, &exclude, breadth.reaps())
                    .await;
            }
        }

        // ── Phase 1c: drop stale symbols ──────────────────────────
        // Bug #23: retrieve_diagnostics gates population on needs_population
        // *alone* (it does not consult symbols_outdated), and Phase 4's
        // bump_generations clears the enrichment cache but not the symbols. So
        // for diagnostics' own enclosing-symbol labels this eager invalidate is
        // load-bearing, not redundant — without it, present-but-stale symbols
        // are served. For any later grep/glob the lazy mtime backstop (bug #26,
        // ensure_symbols) covers the common local-FS case, but this eager path
        // is granularity-independent on the daemon's own write: it clears the
        // symbols unconditionally rather than relying on the on-disk mtime
        // visibly advancing (which a coarse-mtime / NFS / SMB / FUSE mount can
        // defeat). Invalidate here so retrieve re-populates fresh from
        // documentSymbol (files are about to be opened and saved on the server,
        // so it is a cheap request, off the read path). Keep it.
        if let Some(idx_arc) = &self.symbol_index {
            let idx = idx_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in &canonical_paths {
                let _ = idx.invalidate(path);
            }
        }

        // ── Phase 2: per-server batch lifecycle ────────────────────
        // Collect per-file diagnostics across all servers.
        // Key: canonical path string → (display path, Vec<ServerDiagnostics>).
        let mut file_results: BTreeMap<String, (String, Vec<ServerDiagnostics>)> = BTreeMap::new();

        for (client_mutex, paths) in server_groups.values() {
            self.run_server_batch(client_mutex, paths, parent_id, &mut file_results)
                .await;
        }

        // ── Phase 3: classify, budget, and format ────────────────
        let outcome = self.format_output(&canonical_paths, &file_results, &uncovered, session_id);

        // ── Phase 4: invalidate caches ────────────────────────────
        self.fs.bump_generations(&canonical_paths);

        outcome
    }

    /// Classifies files from server results, applies the single-shot preview
    /// budget, writes the overflow file when needed, and reports the
    /// clean/dirty status.
    ///
    /// Root-grouped file entries with diagnostics, `[clean]` markers, or
    /// `[no LSP coverage]` notes. Root headers are collapsed when only one
    /// file exists under that root. On overflow (more diagnostics than the
    /// configured budget) the complete report is written to the per-session
    /// runtime-dir file and the preview ends with a `… N more — full report
    /// at <path>` pointer line.
    fn format_output(
        &self,
        canonical_paths: &[PathBuf],
        file_results: &BTreeMap<String, (String, Vec<ServerDiagnostics>)>,
        uncovered: &[UncoveredEntry],
        session_id: &str,
    ) -> DiagnosticsOutcome {
        let mut diag_files: Vec<DiagnosticFile> = Vec::new();
        let mut clean: Vec<CleanEntry> = Vec::new();

        for cp in canonical_paths {
            let key = cp.to_string_lossy().to_string();
            let segments = file_results.get(&key).map(|(_, segs)| segs.as_slice());
            let display = file_results
                .get(&key)
                .map_or_else(|| self.display_rel(&key), |(d, _)| d.clone());
            let root = self.resolve_root_or_parent(cp);

            match classify_file(segments) {
                FileOutcome::HasDiagnostics(entries) => {
                    diag_files.push(DiagnosticFile {
                        display,
                        root,
                        entries,
                    });
                }
                FileOutcome::Clean | FileOutcome::NoResults => {
                    clean.push(CleanEntry { display, root });
                }
            }
        }

        // Count severities across the complete set (before the preview budget
        // truncates), so the session board's `last_action` reports the real
        // totals (observability ticket 05). Severity 1 = error, 2 = warning.
        let (errors, warnings) =
            diag_files
                .iter()
                .flat_map(|f| &f.entries)
                .fold((0usize, 0usize), |(e, w), entry| match entry.severity {
                    1 => (e + 1, w),
                    2 => (e, w + 1),
                    _ => (e, w),
                });

        // `[tools]` is absent in many configs — fall back to defaults so the
        // budget (50) and dirty threshold (error) always apply.
        let tools = self
            .client_manager
            .config()
            .tools
            .clone()
            .unwrap_or_default();
        let budgeted = budget_diagnostics(
            &diag_files,
            &clean,
            uncovered,
            tools.diagnostics_budget(),
            tools.dirty_severity(),
        );

        let output = if budgeted.overflow_count == 0 {
            budgeted.preview
        } else {
            // Overflow: persist the complete report so the agent can read or
            // `catenary grep` it, and point at it. If the write fails, fall
            // back to the full report inline — losing the tail silently would
            // break the complete-batch guarantee.
            match crate::bridge::overflow::write_diagnostics(
                &crate::paths::runtime_dir(),
                session_id,
                &budgeted.full,
            ) {
                Ok(path) => format!(
                    "{}\u{2026} {} more \u{2014} full report at {}\n",
                    budgeted.preview,
                    budgeted.overflow_count,
                    path.display(),
                ),
                Err(e) => {
                    warn!(
                        session_id = %session_id,
                        "failed to write diagnostics overflow file: {e}",
                    );
                    budgeted.full
                }
            }
        };

        DiagnosticsOutcome {
            output,
            dirty: budgeted.dirty,
            errors,
            warnings,
        }
    }

    /// Runs the batched diagnostics lifecycle on a single server.
    ///
    /// Opens all files, settles, runs health probe if needed,
    /// sends didSave, settles again, retrieves diagnostics per file,
    /// and closes all files. Cleanup runs once regardless of
    /// bail-outs.
    async fn run_server_batch(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        parent_id: Option<&str>,
        file_results: &mut BTreeMap<String, (String, Vec<ServerDiagnostics>)>,
    ) {
        let Some(baseline) = self.pre_open_settle(client_mutex).await else {
            return;
        };

        // The changed-set nudge (Phase 1b) sends didChangeWatchedFiles for
        // externally-changed registered-glob files. Servers may re-scan,
        // discover new files, and emit stale diagnostics (e.g.,
        // rust-analyzer's "unlinked-file" for a .rs file whose parent
        // mod declaration hasn't been seen yet). pre_open_settle waits
        // for the server to go idle after the nudge, but the server's
        // final publishDiagnostics may still be in the kernel pipe
        // buffer — the write syscall completed (so CPU shows idle) but
        // the reader loop hasn't processed the bytes yet. Under load,
        // this gap widens. Drain the pipe first to ensure the cache
        // reflects the server's final state, then clear.
        {
            let server = client_mutex.lock().await.server().clone();
            drain_pipe(&server).await;
        }
        self.clear_stale_diagnostics(client_mutex, paths).await;

        let opened = self.open_files(client_mutex, paths, parent_id).await;
        if opened.is_empty() {
            return;
        }

        // Settle + save + retrieve. Any bail → skip retrieve, still close.
        if self
            .settle_and_save(client_mutex, &opened, baseline)
            .await
            .is_ok()
        {
            // Drain any in-flight publishDiagnostics still in the stdio
            // pipe buffer before reading the diagnostic cache.
            let server = client_mutex.lock().await.server().clone();
            drain_pipe(&server).await;

            self.retrieve_diagnostics(client_mutex, &opened, file_results)
                .await;
        }

        self.close_all(client_mutex, &opened).await;
    }

    /// Settles the server before opening files.
    ///
    /// Waits for the server to go idle (e.g. after
    /// `didChangeWatchedFiles` triggers re-indexing), then samples
    /// baseline ticks for post-open activity detection.
    ///
    /// Returns `None` if the server is dead or dies during settle.
    async fn pre_open_settle(&self, client_mutex: &Arc<Mutex<LspClient>>) -> Option<u64> {
        let client = client_mutex.lock().await;
        if matches!(
            client.lifecycle(),
            ServerLifecycle::Failed | ServerLifecycle::Dead
        ) {
            return None;
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
            return None;
        }

        Some(sample_baseline(&server).await)
    }

    /// Opens all files on the server, collecting their URIs.
    ///
    /// Files that fail to open are logged and skipped.
    async fn open_files(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        parent_id: Option<&str>,
    ) -> Vec<(PathBuf, String)> {
        let mut opened_uris: Vec<(PathBuf, String)> = Vec::new();

        for path in paths {
            match self
                .client_manager
                .open_document_on(path, client_mutex, parent_id.map(str::to_string))
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

        opened_uris
    }

    /// Settles after opens, runs health probe, and sends `didSave`.
    ///
    /// Returns `Ok(())` when the server is ready for retrieval, or
    /// `Err(())` if the server died or a critical step failed (caller
    /// should skip retrieval but still close documents).
    ///
    /// The client lock is held across settle calls so that no other
    /// operation can send requests to the server between stimulus and
    /// idle detection — interleaved traffic would restart activity
    /// and invalidate the settle.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Lock held across settle to prevent interleaved requests"
    )]
    async fn settle_and_save(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        opened_uris: &[(PathBuf, String)],
        post_open_baseline: u64,
    ) -> Result<(), ()> {
        let client = client_mutex.lock().await;

        if matches!(
            client.lifecycle(),
            ServerLifecycle::Failed | ServerLifecycle::Dead
        ) {
            return Err(());
        }

        let server = client.server().clone();
        let server_name = client.server_name().to_string();
        let cancel = CancellationToken::new();

        if !settle_after(
            &server,
            post_open_baseline,
            cancel.clone(),
            &server_name,
            "post-open",
        )
        .await
            || matches!(
                client.lifecycle(),
                ServerLifecycle::Failed | ServerLifecycle::Dead
            )
        {
            return Err(());
        }

        // ── Health probe ──────────────────────────────────────────
        if client.lifecycle() == ServerLifecycle::Probing
            && !client.run_health_probe(&opened_uris[0].1).await
        {
            return Err(());
        }

        // ── didSave all ───────────────────────────────────────────
        if client.wants_did_save() {
            let baseline = sample_baseline(&server).await;

            for (_, uri) in opened_uris {
                if let Err(e) = client.did_save(uri).await {
                    warn!(
                        server = %server_name,
                        "batch didSave failed: {e}",
                    );
                    return Err(());
                }
            }

            if !settle_after(&server, baseline, cancel, &server_name, "post-didSave").await
                || matches!(
                    client.lifecycle(),
                    ServerLifecycle::Failed | ServerLifecycle::Dead
                )
            {
                return Err(());
            }
        }

        Ok(())
    }

    /// Retrieves diagnostics for each opened file on the server.
    ///
    /// Collects push-cached or pull diagnostics, applies severity
    /// filters, fetches quick-fix code actions, populates the symbol
    /// index, and formats entries into `file_results`.
    async fn retrieve_diagnostics(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        opened_uris: &[(PathBuf, String)],
        file_results: &mut BTreeMap<String, (String, Vec<ServerDiagnostics>)>,
    ) {
        let client = client_mutex.lock().await;

        let server_command = client.server_command().to_string();
        let server_name = client.server_name().to_string();
        let server_version = client.server_version().map(str::to_string);
        let lang_id = client.language().to_string();
        let has_code_actions = client.supports_code_action();

        for (path, uri) in opened_uris {
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
                let needs = idx_arc.lock().is_ok_and(|idx| idx.needs_population(path));
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
    }

    /// Clears diagnostics cache entries for files about to be opened.
    ///
    /// The readdir nudge (Phase 1b) may cause the server to emit
    /// `publishDiagnostics` for files it discovers on disk — e.g.,
    /// rust-analyzer's "unlinked-file" for a new `.rs` file before
    /// the parent `mod` declaration is visible. Clearing the cache
    /// after `pre_open_settle` ensures only fresh diagnostics from
    /// the batch settle phase survive to retrieval.
    async fn clear_stale_diagnostics(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
    ) {
        let uris: Vec<String> = paths
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .map(|p| crate::lsp::lang::path_to_uri(&p))
            .collect();
        let uri_refs: Vec<&str> = uris.iter().map(String::as_str).collect();
        client_mutex.lock().await.clear_diagnostics_for(&uri_refs);
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
}

/// Samples cumulative ticks for use as an [`IdleDetector::after_activity`]
/// baseline. Returns 0 if the tree monitor is unavailable.
async fn sample_baseline(server: &Arc<LspServer>) -> u64 {
    let s = Arc::clone(server);
    tokio::task::spawn_blocking(move || s.sample_tree().map_or(0, |snap| snap.cumulative_ticks))
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

/// Drains in-flight notifications from the stdout pipe buffer after
/// settle.
///
/// The settle detector sees the server process tree go idle when CPU
/// deltas reach zero. But the server's final notifications may still
/// be in the kernel pipe buffer — the `write` syscall completed (so
/// CPU shows idle) but the reader loop hasn't processed the bytes yet.
///
/// Injects a sentinel response into the pipe's write end (kept from
/// spawn time) and waits for the reader loop to deliver it. FIFO pipe
/// ordering guarantees that every preceding byte — including any final
/// `publishDiagnostics` — has been processed when the sentinel arrives.
///
/// Without this, `retrieve_diagnostics` can read stale diagnostics
/// from an intermediate analysis phase (e.g., rust-analyzer's
/// fast-check results that are later clobbered by fly-check).
async fn drain_pipe(server: &LspServer) {
    if let Err(e) = server.drain().await {
        debug!("drain_pipe: {e}");
    }
}

/// Stat-walks a workspace root, returning every regular file as a
/// `(root-relative path, mtime)` pair for the WS31 changed-set baseline diff.
///
/// Respects `.gitignore` and skips hidden files (the same scope as the grep
/// walk and `detect_workspace_languages`). Unlike `grep`, `diagnostics` reads
/// the server's index rather than file contents, so this is a dedicated
/// stat-walk — the per-file `mtime` is the only thing read. The manager scopes
/// the result to the union of registered watch globs before diffing.
fn stat_walk(root: &std::path::Path) -> Vec<(PathBuf, i64)> {
    let mut observed = Vec::new();
    let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        if let Ok(md) = path.metadata() {
            observed.push((rel.to_path_buf(), mtime_nanos(&md)));
        }
    }
    observed
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

/// Classifies a file based on its server diagnostics results.
///
/// - `Some(segments)` with any non-empty entries → [`FileOutcome::HasDiagnostics`]
/// - `Some(segments)` with all entries empty → [`FileOutcome::Clean`]
/// - `None` (no server produced results) → [`FileOutcome::NoResults`]
fn classify_file(segments: Option<&[ServerDiagnostics]>) -> FileOutcome {
    let Some(segments) = segments else {
        return FileOutcome::NoResults;
    };

    let entries: Vec<DiagEntry> = segments
        .iter()
        .flat_map(|s| s.entries.iter().cloned())
        .collect();

    if entries.is_empty() {
        FileOutcome::Clean
    } else {
        FileOutcome::HasDiagnostics(entries)
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
) -> Vec<DiagEntry> {
    diagnostics
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let severity_num = crate::lsp::extract::diagnostic_severity(d);
            let severity = match severity_num {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                Some(4) => "hint",
                _ => "unknown",
            };
            // Numeric severity for budgeting/dirty: 1..=4 as-is, anything
            // else (including a missing severity) ranks last and never gates.
            let severity_rank = severity_num.filter(|s| (1..=4).contains(s)).unwrap_or(5);
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

            Some(DiagEntry {
                severity: severity_rank,
                text: result,
            })
        })
        .collect()
}

/// Formats the full diagnostics output.
///
/// Bare root-path section headers. Clean files listed inline with
/// `[clean]`. Uncovered files noted with `[no LSP coverage]`.
///
/// When a root contains a single file, the root and filename are
/// collapsed into one path (e.g. `/tmp/scratch.sh`). Multi-file
/// roots get a directory header with indented file entries beneath.
fn format_diagnostics(
    diag_files: &[DiagnosticFile],
    clean: &[CleanEntry],
    uncovered: &[UncoveredEntry],
) -> String {
    use std::fmt::Write;

    let mut root_diag: BTreeMap<&PathBuf, Vec<(&str, &[DiagEntry])>> = BTreeMap::new();
    let mut root_clean: BTreeMap<&PathBuf, Vec<&str>> = BTreeMap::new();
    let mut root_uncovered: BTreeMap<&PathBuf, Vec<&str>> = BTreeMap::new();

    for df in diag_files {
        root_diag
            .entry(&df.root)
            .or_default()
            .push((&df.display, &df.entries));
    }
    for ce in clean {
        root_clean.entry(&ce.root).or_default().push(&ce.display);
    }
    for ue in uncovered {
        root_uncovered
            .entry(&ue.root)
            .or_default()
            .push(&ue.display);
    }

    let mut all_roots: BTreeSet<&PathBuf> = BTreeSet::new();
    all_roots.extend(root_diag.keys());
    all_roots.extend(root_clean.keys());
    all_roots.extend(root_uncovered.keys());

    let mut output = String::new();

    for root in &all_roots {
        let diag_count = root_diag.get(root).map_or(0, Vec::len);
        let clean_count = root_clean.get(root).map_or(0, Vec::len);
        let uncovered_count = root_uncovered.get(root).map_or(0, Vec::len);
        let total = diag_count + clean_count + uncovered_count;
        let collapsed = total == 1;

        if !output.is_empty() {
            output.push('\n');
        }

        if collapsed {
            // Single file: merge root and filename into one path.
            if let Some(files) = root_diag.get(root) {
                for (display, entries) in files {
                    _ = writeln!(output, "{}:", root.join(display).display());
                    for entry in *entries {
                        for line in entry.text.lines() {
                            _ = writeln!(output, "\t{line}");
                        }
                    }
                }
            }
            if let Some(clean_files) = root_clean.get(root) {
                for f in clean_files {
                    _ = writeln!(output, "{}", root.join(f).display());
                    _ = writeln!(output, "\t[clean]");
                }
            }
            if let Some(uncov_files) = root_uncovered.get(root) {
                for f in uncov_files {
                    _ = writeln!(output, "{}", root.join(f).display());
                    _ = writeln!(output, "\t[no LSP coverage]");
                }
            }
        } else {
            // Multiple files: directory header with indented entries.
            _ = writeln!(output, "{}", root.display());
            if let Some(files) = root_diag.get(root) {
                for (display, entries) in files {
                    _ = writeln!(output, "\t{display}:");
                    for entry in *entries {
                        for line in entry.text.lines() {
                            _ = writeln!(output, "\t\t{line}");
                        }
                    }
                }
            }
            if let Some(clean_files) = root_clean.get(root) {
                for f in clean_files {
                    _ = writeln!(output, "\t{f}");
                    _ = writeln!(output, "\t\t[clean]");
                }
            }
            if let Some(uncov_files) = root_uncovered.get(root) {
                for f in uncov_files {
                    _ = writeln!(output, "\t{f}");
                    _ = writeln!(output, "\t\t[no LSP coverage]");
                }
            }
        }
    }

    output
}

/// Result of applying the single-shot preview budget to a diagnostics run.
struct BudgetedDiagnostics {
    /// The complete report (every diagnostic). Written to the overflow file
    /// when `overflow_count > 0`.
    full: String,
    /// The budgeted preview (first N diagnostics, errors before warnings). Equal
    /// to `full` when nothing overflowed. Carries no overflow pointer line — the
    /// caller appends it once it knows the written path.
    preview: String,
    /// Diagnostics dropped from the preview (`total - budget`), or `0` when the
    /// full set fit.
    overflow_count: usize,
    /// `true` when at least one diagnostic met `dirty_threshold`.
    dirty: bool,
}

/// Apply the errors-first single-shot budget to the classified diagnostics.
///
/// Pure: renders both the complete report and a preview capped at `budget`
/// diagnostics, with errors selected before warnings so a truncation never
/// hides an error behind a warning. Within each file the surviving entries keep
/// their original position order. Clean / uncovered files are not budgeted —
/// they are one line each and always shown. `dirty` is `true` when any
/// diagnostic's severity meets `dirty_threshold` (LSP encoding: lower = more
/// severe).
fn budget_diagnostics(
    diag_files: &[DiagnosticFile],
    clean: &[CleanEntry],
    uncovered: &[UncoveredEntry],
    budget: usize,
    dirty_threshold: u8,
) -> BudgetedDiagnostics {
    let full = format_diagnostics(diag_files, clean, uncovered);

    let dirty = diag_files
        .iter()
        .flat_map(|f| &f.entries)
        .any(|e| crate::filter::severity_passes(e.severity, dirty_threshold));

    let total: usize = diag_files.iter().map(|f| f.entries.len()).sum();

    if total <= budget {
        return BudgetedDiagnostics {
            preview: full.clone(),
            full,
            overflow_count: 0,
            dirty,
        };
    }

    // Globally select the `budget` most-severe entries (stable → ties keep
    // document order), then rebuild each file with its selected entries in
    // original position order. `sort_by_key` is stable, so errors come first
    // and equal-severity entries retain their (file, position) order.
    let mut ranked: Vec<(usize, usize)> = diag_files
        .iter()
        .enumerate()
        .flat_map(|(fi, f)| f.entries.iter().enumerate().map(move |(ei, _)| (fi, ei)))
        .collect();
    ranked.sort_by_key(|&(fi, ei)| diag_files[fi].entries[ei].severity);
    let selected: HashSet<(usize, usize)> = ranked.into_iter().take(budget).collect();

    let preview_files: Vec<DiagnosticFile> = diag_files
        .iter()
        .enumerate()
        .filter_map(|(fi, f)| {
            let entries: Vec<DiagEntry> = f
                .entries
                .iter()
                .enumerate()
                .filter(|(ei, _)| selected.contains(&(fi, *ei)))
                .map(|(_, e)| e.clone())
                .collect();
            (!entries.is_empty()).then(|| DiagnosticFile {
                display: f.display.clone(),
                root: f.root.clone(),
                entries,
            })
        })
        .collect();

    BudgetedDiagnostics {
        preview: format_diagnostics(&preview_files, clean, uncovered),
        full,
        overflow_count: total - budget,
        dirty,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Build a [`DiagEntry`] from a severity and text (test ergonomics).
    fn de(severity: u8, text: &str) -> DiagEntry {
        DiagEntry {
            severity,
            text: text.to_string(),
        }
    }

    // ── classify_file tests ─────────────────────────────────────

    #[test]
    fn classify_file_with_diagnostics() {
        let segments = vec![ServerDiagnostics {
            entries: vec![de(1, ":1:1 [error] test: msg")],
        }];
        assert_eq!(
            classify_file(Some(&segments)),
            FileOutcome::HasDiagnostics(vec![de(1, ":1:1 [error] test: msg")]),
        );
    }

    #[test]
    fn classify_file_empty_entries_is_clean() {
        let segments = vec![ServerDiagnostics { entries: vec![] }];
        assert_eq!(classify_file(Some(&segments)), FileOutcome::Clean);
    }

    #[test]
    fn classify_file_no_results() {
        assert_eq!(classify_file(None), FileOutcome::NoResults);
    }

    #[test]
    fn classify_file_multi_server_merges_entries() {
        let segments = vec![
            ServerDiagnostics {
                entries: vec![de(1, ":1:1 [error] server-a: msg")],
            },
            ServerDiagnostics {
                entries: vec![de(2, ":2:1 [warning] server-b: msg")],
            },
        ];
        assert_eq!(
            classify_file(Some(&segments)),
            FileOutcome::HasDiagnostics(vec![
                de(1, ":1:1 [error] server-a: msg"),
                de(2, ":2:1 [warning] server-b: msg"),
            ]),
        );
    }

    #[test]
    fn classify_file_some_servers_empty() {
        // One server has entries, one doesn't → HasDiagnostics.
        let segments = vec![
            ServerDiagnostics { entries: vec![] },
            ServerDiagnostics {
                entries: vec![de(1, ":1:1 [error] test: msg")],
            },
        ];
        assert_eq!(
            classify_file(Some(&segments)),
            FileOutcome::HasDiagnostics(vec![de(1, ":1:1 [error] test: msg")]),
        );
    }

    #[test]
    fn classify_file_all_servers_empty_is_clean() {
        // Multiple servers, all empty → Clean.
        let segments = vec![
            ServerDiagnostics { entries: vec![] },
            ServerDiagnostics { entries: vec![] },
        ];
        assert_eq!(classify_file(Some(&segments)), FileOutcome::Clean);
    }

    // ── format_diagnostics tests ────────────────────────────────────

    #[test]
    fn format_single_file_with_diagnostics() {
        let diag_files = vec![DiagnosticFile {
            display: "file.rs".to_string(),
            root: PathBuf::from("/test"),
            entries: vec![de(1, ":1:1 [error] test: msg")],
        }];
        let output = format_diagnostics(&diag_files, &[], &[]);
        assert!(!output.contains("[LSP available]"), "output: {output}");
        // Single file under root → collapsed path.
        assert!(output.contains("/test/file.rs:"), "output: {output}");
        assert!(output.contains("\t:1:1 [error]"), "output: {output}");
    }

    #[test]
    fn format_all_entries_shown() {
        let entries: Vec<DiagEntry> = (0..5)
            .map(|i| de(2, &format!(":{i}:1 [warning] test: msg {i}")))
            .collect();
        let diag_files = vec![DiagnosticFile {
            display: "file.rs".to_string(),
            root: PathBuf::from("/test"),
            entries,
        }];
        let output = format_diagnostics(&diag_files, &[], &[]);
        // All entries should be present (no paging).
        for i in 0..5 {
            assert!(output.contains(&format!("msg {i}")), "output: {output}");
        }
    }

    #[test]
    fn format_clean_file() {
        let clean = vec![CleanEntry {
            display: "clean.rs".to_string(),
            root: PathBuf::from("/test"),
        }];
        let output = format_diagnostics(&[], &clean, &[]);
        // Single file → collapsed path with [clean].
        assert!(output.contains("/test/clean.rs\n"), "output: {output}");
        assert!(output.contains("\t[clean]"), "output: {output}");
        assert!(!output.contains("N/A:"), "output: {output}");
    }

    #[test]
    fn format_multi_root_grouping() {
        let diag_files = vec![
            DiagnosticFile {
                display: "src/lib.rs".to_string(),
                root: PathBuf::from("/alpha"),
                entries: vec![de(1, ":1:1 [error] test: alpha error")],
            },
            DiagnosticFile {
                display: "src/lib.rs".to_string(),
                root: PathBuf::from("/beta"),
                entries: vec![de(2, ":5:1 [warning] test: beta warning")],
            },
        ];
        let clean = vec![CleanEntry {
            display: "src/main.rs".to_string(),
            root: PathBuf::from("/alpha"),
        }];
        let output = format_diagnostics(&diag_files, &clean, &[]);
        // /alpha has 2 files (diag + clean) → expanded with directory header.
        let alpha_pos = output.find("/alpha\n").expect("missing /alpha header");
        assert!(output.contains("\tsrc/lib.rs:"), "output: {output}");
        assert!(output.contains("\t\t:1:1 [error]"), "output: {output}");
        assert!(output.contains("\tsrc/main.rs\n"), "output: {output}");
        assert!(output.contains("\t\t[clean]"), "output: {output}");
        // /beta has 1 file → collapsed into single path.
        let beta_pos = output
            .find("/beta/src/lib.rs:")
            .expect("missing /beta collapsed path");
        assert!(alpha_pos < beta_pos, "output: {output}");
        assert!(output.contains("beta warning"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
    }

    #[test]
    fn format_single_file_server() {
        let diag_files = vec![DiagnosticFile {
            display: "scratch.sh".to_string(),
            root: PathBuf::from("/tmp"),
            entries: vec![de(2, ":3:1 [warning] test: standalone warning")],
        }];
        let output = format_diagnostics(&diag_files, &[], &[]);
        // Single file → collapsed path.
        assert!(output.contains("/tmp/scratch.sh:"), "output: {output}");
        assert!(output.contains("\t:3:1 [warning]"), "output: {output}");
        assert!(output.contains("standalone warning"), "output: {output}");
        assert!(!output.contains("OutOfRoots:"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
        assert!(!output.contains("N/A:"), "output: {output}");
    }

    #[test]
    fn format_no_lsp_header() {
        let diag_files = vec![DiagnosticFile {
            display: "file.rs".to_string(),
            root: PathBuf::from("/test"),
            entries: vec![de(1, ":1:1 [error] test: msg")],
        }];
        let output = format_diagnostics(&diag_files, &[], &[]);
        // No status header — output starts directly with file content.
        assert!(!output.contains("[LSP available]"), "output: {output}");
        // Bare path, no prefix.
        assert!(output.contains("/test/file.rs:"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
    }

    #[test]
    fn format_uncovered_file() {
        let uncovered = vec![UncoveredEntry {
            display: "data.csv".to_string(),
            root: PathBuf::from("/project"),
        }];
        let output = format_diagnostics(&[], &[], &uncovered);
        // Single file → collapsed path with [no LSP coverage].
        assert!(output.contains("/project/data.csv\n"), "output: {output}");
        assert!(output.contains("\t[no LSP coverage]"), "output: {output}");
    }

    #[test]
    fn format_mixed_clean_and_uncovered() {
        let clean = vec![CleanEntry {
            display: "lib.rs".to_string(),
            root: PathBuf::from("/project"),
        }];
        let uncovered = vec![UncoveredEntry {
            display: "data.csv".to_string(),
            root: PathBuf::from("/project"),
        }];
        let output = format_diagnostics(&[], &clean, &uncovered);
        // Two files under same root → expanded with directory header.
        assert!(output.contains("/project\n"), "output: {output}");
        assert!(output.contains("\tlib.rs\n"), "output: {output}");
        assert!(output.contains("\t\t[clean]"), "output: {output}");
        assert!(output.contains("\tdata.csv\n"), "output: {output}");
        assert!(output.contains("\t\t[no LSP coverage]"), "output: {output}");
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
        assert_eq!(entries[0].severity, 2, "warning severity");
        // 0-indexed (15, 5) → 1-indexed (16, 6)
        assert!(
            entries[0].text.starts_with(":16:6 "),
            "line/col: {}",
            entries[0].text
        );
        assert!(
            entries[0].text.contains("[warning]"),
            "severity: {}",
            entries[0].text
        );
        assert!(
            entries[0].text.ends_with("(in my_function)"),
            "entry: {}",
            entries[0].text
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
    fn format_diagnostics_entries_all_severity_labels() {
        let filter = crate::filter::get_filter("");
        for (sev, label) in [(1, "error"), (2, "warning"), (3, "info"), (4, "hint")] {
            let diags = vec![make_diag(0, 0, sev, "msg")];
            let entries =
                format_diagnostics_entries(&diags, &[], filter, "test", None, "rust", &[]);
            assert_eq!(entries.len(), 1, "severity {sev}");
            assert_eq!(entries[0].severity, sev, "numeric severity {sev}");
            assert!(
                entries[0].text.contains(&format!("[{label}]")),
                "severity {sev}: {}",
                entries[0].text
            );
        }
    }

    #[test]
    fn format_diagnostics_entries_with_code() {
        let diag = serde_json::json!({
            "range": {
                "start": { "line": 3, "character": 7 },
                "end": { "line": 3, "character": 10 }
            },
            "severity": 1,
            "source": "rustc",
            "code": "E0308",
            "message": "mismatched types"
        });
        let filter = crate::filter::get_filter("");
        let entries = format_diagnostics_entries(&[diag], &[], filter, "test", None, "rust", &[]);
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].text.starts_with(":4:8 "),
            "line/col: {}",
            entries[0].text
        );
        assert!(
            entries[0].text.contains("rustc(E0308)"),
            "source(code): {}",
            entries[0].text
        );
    }

    #[test]
    fn resolve_path_absolute_unchanged() {
        let result = resolve_path("/some/absolute/path").expect("should resolve");
        assert_eq!(result, PathBuf::from("/some/absolute/path"));
    }

    #[test]
    fn resolve_path_relative_prepends_cwd() {
        let result = resolve_path("relative/file.rs").expect("should resolve");
        assert!(
            result.is_absolute(),
            "result should be absolute: {}",
            result.display()
        );
        assert!(
            result.ends_with("relative/file.rs"),
            "result should end with relative path: {}",
            result.display()
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
        // 0-indexed (10, 5) → 1-indexed (11, 6)
        assert!(
            with_empty[0].text.starts_with(":11:6 "),
            "line/col: {}",
            with_empty[0].text
        );
        assert!(
            with_empty[0].text.contains("[error]"),
            "severity: {}",
            with_empty[0].text
        );
        assert!(
            with_empty[0].text.contains("some error"),
            "message: {}",
            with_empty[0].text
        );
        assert!(
            !with_empty[0].text.contains("(in "),
            "no symbol suffix: {}",
            with_empty[0].text
        );
    }

    // ── budget + overflow tests ─────────────────────────────────────

    #[test]
    fn budget_diagnostics_under_budget_no_overflow() {
        let diag_files = vec![DiagnosticFile {
            display: "a.rs".to_string(),
            root: PathBuf::from("/r"),
            entries: vec![de(1, ":1:1 [error] e: one"), de(2, ":2:1 [warning] w: two")],
        }];
        let b = budget_diagnostics(&diag_files, &[], &[], 50, 1);
        assert_eq!(b.overflow_count, 0);
        assert_eq!(b.preview, b.full);
        assert!(b.dirty, "an error is dirty at threshold error");
        assert!(b.preview.contains("one") && b.preview.contains("two"));
    }

    #[test]
    fn budget_diagnostics_errors_before_warnings() {
        // warning, error, warning — a budget of 1 must keep the error.
        let diag_files = vec![DiagnosticFile {
            display: "a.rs".to_string(),
            root: PathBuf::from("/r"),
            entries: vec![
                de(2, ":1:1 [warning] w: warn-a"),
                de(1, ":2:1 [error] e: err-b"),
                de(2, ":3:1 [warning] w: warn-c"),
            ],
        }];
        let b = budget_diagnostics(&diag_files, &[], &[], 1, 1);
        assert_eq!(b.overflow_count, 2);
        assert!(b.preview.contains("err-b"), "error survives: {}", b.preview);
        assert!(
            !b.preview.contains("warn-a"),
            "warning dropped: {}",
            b.preview
        );
        assert!(
            !b.preview.contains("warn-c"),
            "warning dropped: {}",
            b.preview
        );
        // The complete set is preserved in `full`.
        assert!(
            b.full.contains("warn-a") && b.full.contains("err-b") && b.full.contains("warn-c"),
            "full keeps everything: {}",
            b.full
        );
    }

    #[test]
    fn budget_diagnostics_dirty_threshold() {
        let diag_files = vec![DiagnosticFile {
            display: "a.rs".to_string(),
            root: PathBuf::from("/r"),
            entries: vec![de(2, ":1:1 [warning] w: warn")],
        }];
        // Warnings-only is clean at the default error threshold.
        assert!(!budget_diagnostics(&diag_files, &[], &[], 50, 1).dirty);
        // ...but dirty when the threshold is lowered to warning.
        assert!(budget_diagnostics(&diag_files, &[], &[], 50, 2).dirty);
    }
}
