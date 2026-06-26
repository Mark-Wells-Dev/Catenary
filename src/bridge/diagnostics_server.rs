// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, idle detection, diagnostics retrieval (push cache
//! first, pull fallback), severity filtering, noise filtering, quick-fix
//! collection, and compact formatting.

use super::filesystem_manager::{FilesystemManager, observe_mtime};
use super::linter::DiagnosticFeeder;
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

/// Rendering context shared by every [`FeederEntry`] from one feeder.
///
/// A feeder is a single diagnostic source for a batch: one language server, or
/// one standalone linter. The fields drive the message filter and the rendered
/// `source(code)` line; they are constant across a feeder's diagnostics, so the
/// context is shared behind an `Arc` rather than copied per entry.
#[derive(Debug)]
struct FeederContext {
    /// The feeder's command — the LSP server command or the linter command.
    /// Selects the message filter and feeds version-keyed filtering.
    command: String,
    /// Server version, when known (LSP feeders only; `None` for linters).
    version: Option<String>,
    /// Language id for the message filter (LSP feeders only; empty for linters).
    language_id: String,
}

/// One LSP-shaped diagnostic from a single feeder, before the cross-feeder
/// merge (workstream 34 ticket 02).
///
/// Feeders publish these; the per-file aggregation pass dedups and reconciles on
/// the raw `value` (`source` / `code` / start-line), then renders each survivor
/// through its own [`FeederContext`]. Rendering is deferred to *after* the merge
/// so dedup and precedence see canonical LSP-diagnostic JSON, feeder-blind.
#[derive(Debug)]
struct FeederEntry {
    /// The raw LSP-shaped diagnostic JSON. The dedup key and precedence policy
    /// both read from here.
    value: Value,
    /// Quick-fix titles for this diagnostic (LSP code actions). Empty for
    /// linters, which carry no code actions.
    fixes: Vec<String>,
    /// Innermost enclosing symbol name, if resolved (LSP feeders only).
    enclosing: Option<String>,
    /// Shared rendering context for the producing feeder.
    ctx: Arc<FeederContext>,
}

/// A file's accumulated feeder diagnostics, keyed by canonical path in the
/// batch result map.
///
/// Presence of a key means at least one feeder produced a result for the file
/// (so the clean-vs-no-results distinction survives the merge); the `entries`
/// may still be empty when every feeder reported clean.
struct FileFeed {
    /// Display path (root-relative or bare filename) for output.
    display: String,
    /// Raw per-feeder diagnostics, merged across all feeders for the file.
    entries: Vec<FeederEntry>,
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
        clippy::too_many_lines,
        reason = "batch pipeline: the server-grouping map is local; the phased lifecycle (resolve / nudge / LSP / linter / format) reads top-to-bottom"
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
        // The lint-covered subset, fanned out to standalone linters in Phase 2b
        // (workstream 34 ticket 01). A file can be both LSP- and lint-covered.
        let mut lint_candidates: Vec<PathBuf> = Vec::new();

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

            // Suppress the diagnostics surface for `disable_diag` roots (ticket
            // 00). The editing gate already declines to accumulate such files,
            // but filter here too so no other accumulation path (or a
            // mid-session toggle) leaks diagnostics for a surface turned off.
            if self
                .fs
                .resolve_root(&canonical)
                .is_some_and(|root| self.client_manager.is_diag_disabled(&root))
            {
                continue;
            }

            let clients = self.client_manager.diagnostic_servers(&canonical).await;
            // A standalone linter may cover this file too (or instead of LSP).
            let lint_covered = self.client_manager.lint_covers(&canonical);

            if clients.is_empty() {
                if lint_covered {
                    // Lint-only coverage: no language server, but a matching
                    // linter will report. Treat it as covered so it is neither
                    // flagged `[no LSP coverage]` nor dropped, and so any linter
                    // diagnostics render in the format pass.
                    canonical_paths.push(canonical.clone());
                    lint_candidates.push(canonical);
                } else {
                    let display = self.display_rel(&canonical.to_string_lossy());
                    let root = self.resolve_root_or_parent(&canonical);
                    uncovered.push(UncoveredEntry { display, root });
                }
                continue;
            }

            canonical_paths.push(canonical.clone());
            if lint_covered {
                lint_candidates.push(canonical.clone());
            }

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
        // Collect each file's raw LSP-shaped diagnostics across all servers.
        // Key: canonical path string → the file's accumulated feeder entries.
        // Rendering is deferred to Phase 2c so dedup/precedence run on canonical
        // JSON, feeder-blind (ticket 02).
        let mut feeds: BTreeMap<String, FileFeed> = BTreeMap::new();

        for (client_mutex, paths) in server_groups.values() {
            self.run_server_batch(client_mutex, paths, parent_id, &mut feeds)
                .await;
        }

        // ── Phase 2b: linter feeders (workstream 34 ticket 01) ─────
        // Fan the lint-covered subset out to matching standalone linters and
        // merge each adapter's LSP-shaped diagnostics into the same per-file map
        // the LSP pass populated — still raw, for the cross-feeder pass below.
        // Fail-soft: a not-installed linter or a parse failure drops its
        // diagnostics without poisoning the batch.
        if !lint_candidates.is_empty() {
            let feeder = super::linter::LinterFeeder::new(&self.client_manager, &self.fs);
            for feed in feeder.feed(&lint_candidates).await {
                if feed.diagnostics.is_empty() {
                    continue;
                }
                let ctx = Arc::new(FeederContext {
                    command: feed.command,
                    version: None,
                    language_id: String::new(),
                });
                let key = feed.file.to_string_lossy().to_string();
                let display = self.display_rel(&key);
                let file_feed = feeds.entry(key).or_insert_with(|| FileFeed {
                    display,
                    entries: Vec::new(),
                });
                for value in feed.diagnostics {
                    file_feed.entries.push(FeederEntry {
                        value,
                        fixes: Vec::new(),
                        enclosing: None,
                        ctx: Arc::clone(&ctx),
                    });
                }
            }
        }

        // ── Phase 2c: cross-feeder aggregation (ticket 02) ─────────
        // Per file, over the merged set from every feeder: dedup identical
        // findings, reconcile source precedence (per-root policy), then render.
        // This is the order the ticket fixes — merge → dedup → precedence →
        // render → budget/format.
        let file_results = self.aggregate_feeds(feeds);

        // ── Phase 3: classify, budget, and format ────────────────
        let outcome = self.format_output(&canonical_paths, &file_results, &uncovered, session_id);

        // ── Phase 4: invalidate caches ────────────────────────────
        self.fs.bump_generations(&canonical_paths);

        outcome
    }

    /// Cross-feeder aggregation: per file, dedup → reconcile precedence →
    /// render (workstream 34 ticket 02).
    ///
    /// Runs over the merged raw diagnostics each file accumulated from every
    /// feeder (language servers and linters). Dedup is opinion-free (the same
    /// finding from two feeders shown once); precedence is the narrow,
    /// per-root, opinion-laden rule (advisory source dropped in the
    /// authoritative band). Both operate on canonical LSP-diagnostic JSON, so
    /// the pass is feeder-blind. Each surviving entry is then rendered through
    /// its own feeder's context; the per-key presence (even with zero rendered
    /// entries) is preserved so the downstream clean-vs-no-results distinction
    /// survives.
    fn aggregate_feeds(
        &self,
        feeds: BTreeMap<String, FileFeed>,
    ) -> BTreeMap<String, (String, Vec<DiagEntry>)> {
        let mut rendered: BTreeMap<String, (String, Vec<DiagEntry>)> = BTreeMap::new();
        for (key, feed) in feeds {
            let path = PathBuf::from(&key);
            let policies = self.fs.resolve_root(&path).map_or_else(
                || self.client_manager.config().diagnostic_precedence.clone(),
                |root| self.client_manager.effective_precedence(&root),
            );

            let deduped = dedupe_entries(feed.entries);
            let reconciled = reconcile_entries(deduped, &policies);

            let entries: Vec<DiagEntry> = reconciled
                .iter()
                .filter_map(|e| {
                    let filter = crate::filter::get_filter(&e.ctx.command);
                    render_entry(
                        &e.value,
                        &e.fixes,
                        e.enclosing.as_deref(),
                        filter,
                        &e.ctx.command,
                        e.ctx.version.as_deref(),
                        &e.ctx.language_id,
                    )
                })
                .collect();

            rendered.insert(key, (feed.display, entries));
        }
        rendered
    }

    /// Classifies files from server results, applies the single-shot preview
    /// budget, writes the overflow file when needed, and reports the
    /// clean/dirty status.
    ///
    /// Root-grouped file entries with diagnostics, or `[no LSP coverage]`
    /// notes. Clean files are **omitted** — the linter idiom (silent on
    /// success): a fully-clean batch yields empty output (misc 111). Root
    /// headers are collapsed when only one printed file exists under that
    /// root. On overflow (more diagnostics than the configured budget) the
    /// complete report is written to the per-session runtime-dir file and the
    /// preview ends with a `… N more — full report at <path>` pointer line.
    fn format_output(
        &self,
        canonical_paths: &[PathBuf],
        file_results: &BTreeMap<String, (String, Vec<DiagEntry>)>,
        uncovered: &[UncoveredEntry],
        session_id: &str,
    ) -> DiagnosticsOutcome {
        let mut diag_files: Vec<DiagnosticFile> = Vec::new();

        for cp in canonical_paths {
            let key = cp.to_string_lossy().to_string();
            let entries = file_results.get(&key).map(|(_, e)| e.as_slice());
            let display = file_results
                .get(&key)
                .map_or_else(|| self.display_rel(&key), |(d, _)| d.clone());
            let root = self.resolve_root_or_parent(cp);

            match classify_file(entries) {
                FileOutcome::HasDiagnostics(entries) => {
                    diag_files.push(DiagnosticFile {
                        display,
                        root,
                        entries,
                    });
                }
                // Clean and result-less files are silent (misc 111): they
                // produce no per-file line and no root header.
                FileOutcome::Clean | FileOutcome::NoResults => {}
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
        feeds: &mut BTreeMap<String, FileFeed>,
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

            self.retrieve_diagnostics(client_mutex, &opened, feeds)
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

    /// Retrieves raw diagnostics for each opened file on the server and merges
    /// them, unrendered, into `feeds`.
    ///
    /// Collects push-cached or pull diagnostics, applies the per-server severity
    /// filter, fetches quick-fix code actions, populates the symbol index, and
    /// pushes each diagnostic as a [`FeederEntry`] — the raw LSP-shaped JSON plus
    /// its render context. Source-precedence reconciliation no longer runs here:
    /// it is hoisted to the per-file cross-feeder pass (ticket 02) so one policy
    /// reconciles every feeder's findings together. Every opened file is recorded
    /// (even with zero diagnostics) so the clean-vs-no-results distinction
    /// survives the merge.
    async fn retrieve_diagnostics(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        opened_uris: &[(PathBuf, String)],
        feeds: &mut BTreeMap<String, FileFeed>,
    ) {
        let client = client_mutex.lock().await;

        let server_command = client.server_command().to_string();
        let server_name = client.server_name().to_string();
        let server_version = client.server_version().map(str::to_string);
        let lang_id = client.language().to_string();
        let has_code_actions = client.supports_code_action();

        let ctx = Arc::new(FeederContext {
            command: server_command,
            version: server_version,
            language_id: lang_id,
        });

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

            let key = path.to_string_lossy().to_string();
            let display = self.display_rel(&key);
            // Record the file even with zero diagnostics so it classifies as
            // Clean, not NoResults (a server reached retrieval for it).
            let file_feed = feeds.entry(key).or_insert_with(|| FileFeed {
                display,
                entries: Vec::new(),
            });
            for (i, value) in diagnostics.into_iter().enumerate() {
                file_feed.entries.push(FeederEntry {
                    value,
                    fixes: fixes.get(i).cloned().unwrap_or_default(),
                    enclosing: enclosing_symbols.get(i).cloned().flatten(),
                    ctx: Arc::clone(&ctx),
                });
            }
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
///
/// Every enumerated present file is recorded via the shared
/// [`observe_mtime`](super::filesystem_manager::observe_mtime) helper: it
/// retries a transient stat miss and falls back to the
/// [`OBSERVED_STAT_MISS_MTIME`](super::filesystem_manager::OBSERVED_STAT_MISS_MTIME)
/// sentinel — it is **never** omitted. Omitting an enumerated present file would
/// drop it from the observation set, and this result feeds
/// `nudge_changed_set(..., reap=true)` (a `Full` walk for a covered root), so a
/// stat-miss omission would false-reap a live file as `Deleted` (WS31-review
/// F1/H1). The same per-entry contract grep's walker uses.
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
        observed.push((rel.to_path_buf(), observe_mtime(path)));
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

/// Classifies a file based on its rendered cross-feeder diagnostics.
///
/// - `Some(entries)` non-empty → [`FileOutcome::HasDiagnostics`]
/// - `Some(entries)` empty (a feeder reached retrieval but reported clean, or
///   every diagnostic was filtered out) → [`FileOutcome::Clean`]
/// - `None` (no feeder produced a result for the file) → [`FileOutcome::NoResults`]
fn classify_file(entries: Option<&[DiagEntry]>) -> FileOutcome {
    match entries {
        None => FileOutcome::NoResults,
        Some([]) => FileOutcome::Clean,
        Some(entries) => FileOutcome::HasDiagnostics(entries.to_vec()),
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

/// Reconciles overlapping diagnostics by their `source` priority chain (misc
/// 115, bug 42; chain form in linters ticket 02).
///
/// **Generic, feeder-agnostic.** Given a file's complete merged diagnostic set
/// and a [`DiagnosticPrecedence`] chain (source names, highest trust first),
/// drops each diagnostic whose source is outranked by a source that *actually
/// reported* for this file — inside the band when a `code_pattern` is set.
/// Absence of a higher-priority report is **not** contradiction: with no
/// higher-ranked source present, the diagnostic is kept (the instant
/// pre-flycheck preview, a single-source server). Out-of-band diagnostics (a
/// lower source's own lints, an unresolved-import that is not a rustc-coded
/// error) are always kept.
///
/// The gate is the *presence* of a strictly-higher-priority diagnostic in this
/// set — a clean higher source publishes zero diagnostics, so its silence here
/// is indistinguishable from not-yet-reported, and the lower source is kept (no
/// over-suppression). For the rust-analyzer / flycheck case that holds: flycheck
/// publishes its findings for a file it analyzed, and a native phantom E#### only
/// matters when it claims an error flycheck did not.
///
/// Production reconciliation runs over the cross-feeder [`FeederEntry`] set via
/// [`reconcile_entries`]; this value-level form exercises the shared
/// [`precedence_min_rank`] / [`precedence_drops`] predicates directly.
#[cfg(test)]
fn reconcile_source_precedence(
    diagnostics: Vec<Value>,
    precedence: &crate::config::DiagnosticPrecedence,
) -> Vec<Value> {
    let Some(min_rank) = precedence_min_rank(diagnostics.iter(), precedence) else {
        return diagnostics;
    };
    diagnostics
        .into_iter()
        .filter(|d| !precedence_drops(d, precedence, min_rank))
        .collect()
}

/// The trust rank of a diagnostic's `source` within the chain, or `None` when
/// the source is not part of it.
fn source_rank(
    diagnostic: &Value,
    precedence: &crate::config::DiagnosticPrecedence,
) -> Option<usize> {
    diagnostic
        .get("source")
        .and_then(Value::as_str)
        .and_then(|s| precedence.rank(s))
}

/// The best (lowest) rank present across the set — the highest-trust source that
/// reported for the file. `None` when no charted source reported, in which case
/// nothing is dropped (the gate never fires).
fn precedence_min_rank<'a>(
    diagnostics: impl IntoIterator<Item = &'a Value>,
    precedence: &crate::config::DiagnosticPrecedence,
) -> Option<usize> {
    diagnostics
        .into_iter()
        .filter_map(|d| source_rank(d, precedence))
        .min()
}

/// Whether a diagnostic is outranked (some strictly-higher source reported) and
/// in-band — the condition for dropping it. `min_rank` is the best rank present
/// in the file's set; a diagnostic at rank `r > min_rank` has a higher-priority
/// source present, so it loses inside the band.
fn precedence_drops(
    diagnostic: &Value,
    precedence: &crate::config::DiagnosticPrecedence,
    min_rank: usize,
) -> bool {
    match source_rank(diagnostic, precedence) {
        Some(r) if r > min_rank => {
            precedence.code_in_band(&render_diagnostic_code(diagnostic.get("code")))
        }
        _ => false,
    }
}

/// Reconciles source precedence over a file's merged cross-feeder set (ticket
/// 02).
///
/// Applies each per-root [`DiagnosticPrecedence`](crate::config::DiagnosticPrecedence)
/// chain in turn to the [`FeederEntry`] list — the same outranked-in-band rule
/// the per-server path used, now run over diagnostics from *every* feeder
/// together. Chains are narrow and source-disjoint in practice, so the order
/// among them does not matter.
fn reconcile_entries(
    mut entries: Vec<FeederEntry>,
    policies: &[crate::config::DiagnosticPrecedence],
) -> Vec<FeederEntry> {
    for policy in policies {
        let Some(min_rank) = precedence_min_rank(entries.iter().map(|e| &e.value), policy) else {
            continue;
        };
        entries.retain(|e| !precedence_drops(&e.value, policy, min_rank));
    }
    entries
}

/// Collapses identical findings delivered by more than one feeder (ticket 02).
///
/// Opinion-free dedup keyed coarse on `(source, code, start-line)` — anchored on
/// line, not column/span, since LSP (0-based char) and CLI (1-based) ranges
/// drift and a wrapper may normalize spans differently. A codeless diagnostic
/// falls back to `(source, normalized-message, line)`, best-effort. First
/// occurrence wins (LSP feeders populate before linters), and the bias is
/// **coarse**: over-dedup on a tie beats leaking duplicates, since the
/// aggregator owns the clean output.
///
/// Reliable because a wrapped tool preserves its identity: bash-language-server
/// runs shellcheck and emits `source: "shellcheck", code: "SC2086"`, exactly
/// what standalone shellcheck emits — so the same finding collapses regardless
/// of which feeder delivered it.
fn dedupe_entries(entries: Vec<FeederEntry>) -> Vec<FeederEntry> {
    let mut seen: HashSet<(String, String, u32)> = HashSet::new();
    entries
        .into_iter()
        .filter(|e| seen.insert(dedup_key(&e.value)))
        .collect()
}

/// Builds the coarse dedup key for a diagnostic: `(source, discriminant, line)`.
///
/// `line` is the 0-based start line. The discriminant is the rendered code when
/// present (`c\0<code>`), else the normalized message (`m\0<message>`) — the NUL
/// tag keeps a code that happens to equal a message text from colliding across
/// the two key shapes.
fn dedup_key(diagnostic: &Value) -> (String, String, u32) {
    let source = diagnostic
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let line = crate::lsp::extract::diagnostic_range(diagnostic).map_or(0, |r| r.start.line);
    let code = render_diagnostic_code(diagnostic.get("code"));
    let discriminant = if code.is_empty() {
        let message = crate::lsp::extract::diagnostic_message(diagnostic).unwrap_or("");
        format!("m\u{0}{}", normalize_message(message))
    } else {
        format!("c\u{0}{code}")
    };
    (source, discriminant, line)
}

/// Normalizes a diagnostic message for codeless dedup: trims, collapses internal
/// whitespace runs to a single space, and lowercases. Best-effort — a wrapper
/// that rephrases the message defeats it, which is why codes are preferred.
fn normalize_message(message: &str) -> String {
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Renders a diagnostic's JSON `code` field to a string for band and dedup
/// matching.
///
/// Mirrors the rendering [`render_entry`] uses: an integer code becomes its
/// decimal form, a string code is taken as-is. A missing code is the empty
/// string (which only matches an empty/absent band pattern).
fn render_diagnostic_code(code: Option<&Value>) -> String {
    code.map(|c| {
        c.as_i64().map_or_else(
            || c.as_str().map_or_else(|| c.to_string(), str::to_string),
            |n| n.to_string(),
        )
    })
    .unwrap_or_default()
}

/// Renders one LSP-shaped diagnostic into a [`DiagEntry`], or `None` when the
/// message filter drops it.
///
/// The entry text carries the line/column, severity label, `source(code)`,
/// message, optional enclosing-symbol suffix, and any indented quick-fix lines.
/// This is the single rendering path for every feeder — the cross-feeder
/// aggregation pass (ticket 02) calls it once per surviving entry, each through
/// its own feeder's `filter`/`server_command`/`server_version`/`language_id`.
///
/// `fixes` are the quick-fix titles for this diagnostic (empty for linters);
/// `enclosing` is the innermost enclosing symbol name, if resolved.
fn render_entry(
    diagnostic: &Value,
    fixes: &[String],
    enclosing: Option<&str>,
    filter: &dyn crate::filter::DiagnosticFilter,
    server_command: &str,
    server_version: Option<&str>,
    language_id: &str,
) -> Option<DiagEntry> {
    let severity_num = crate::lsp::extract::diagnostic_severity(diagnostic);
    let severity = match severity_num {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "unknown",
    };
    // Numeric severity for budgeting/dirty: 1..=4 as-is, anything else
    // (including a missing severity) ranks last and never gates.
    let severity_rank = severity_num.filter(|s| (1..=4).contains(s)).unwrap_or(5);
    let (line, col) = crate::lsp::extract::diagnostic_range(diagnostic)
        .map_or((0, 0), |r| (r.start.line + 1, r.start.character + 1));
    let source = diagnostic.get("source").and_then(Value::as_str);
    let source_str = source.unwrap_or("");
    let code_value = diagnostic.get("code");
    let code = render_diagnostic_code(code_value);

    let diag_code = code_value.map(crate::filter::DiagnosticCode::from_value);
    let message = filter.filter_message(
        server_command,
        server_version,
        source,
        diag_code.as_ref(),
        crate::lsp::extract::diagnostic_severity(diagnostic)
            .unwrap_or(crate::filter::SEVERITY_WARNING),
        language_id,
        crate::lsp::extract::diagnostic_message(diagnostic).unwrap_or(""),
    );

    // Empty message means the filter wants to drop this diagnostic.
    if message.is_empty() {
        return None;
    }

    let mut result = if code.is_empty() {
        format!(":{line}:{col} [{severity}] {source_str}: {message}")
    } else {
        format!(":{line}:{col} [{severity}] {source_str}({code}): {message}")
    };

    // Append enclosing symbol context.
    if let Some(name) = enclosing {
        use std::fmt::Write;
        let _ = write!(result, " (in {name})");
    }

    // Append indented fix lines.
    for title in fixes {
        use std::fmt::Write;
        let _ = write!(result, "\n\tfix: {title}");
    }

    Some(DiagEntry {
        severity: severity_rank,
        text: result,
    })
}

/// Renders a slice of diagnostics through [`render_entry`] (test ergonomics).
///
/// `fixes` and `enclosing_symbols` are parallel to `diagnostics`; pass empty
/// slices when none were collected. Production code renders one entry at a time
/// in the cross-feeder pass, so this batch helper is test-only.
#[cfg(test)]
fn format_diagnostics_entries(
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
            render_entry(
                d,
                fixes.get(i).map_or(&[], Vec::as_slice),
                enclosing_symbols.get(i).and_then(Option::as_deref),
                filter,
                server_command,
                server_version,
                language_id,
            )
        })
        .collect()
}

/// Formats the diagnostics output.
///
/// Bare root-path section headers. Files with diagnostics are listed;
/// uncovered files noted with `[no LSP coverage]`. Clean files are
/// **omitted entirely** — the linter idiom (silent on success): a
/// fully-clean batch renders an empty string (misc 111). Clean files
/// therefore neither produce a line nor count toward the collapse total.
///
/// When a root contains a single (printed) file, the root and filename
/// are collapsed into one path (e.g. `/tmp/scratch.sh`). Multi-file
/// roots get a directory header with indented file entries beneath.
/// Root headers are only emitted for roots that have something to print.
fn format_diagnostics(diag_files: &[DiagnosticFile], uncovered: &[UncoveredEntry]) -> String {
    use std::fmt::Write;

    let mut root_diag: BTreeMap<&PathBuf, Vec<(&str, &[DiagEntry])>> = BTreeMap::new();
    let mut root_uncovered: BTreeMap<&PathBuf, Vec<&str>> = BTreeMap::new();

    for df in diag_files {
        root_diag
            .entry(&df.root)
            .or_default()
            .push((&df.display, &df.entries));
    }
    for ue in uncovered {
        root_uncovered
            .entry(&ue.root)
            .or_default()
            .push(&ue.display);
    }

    let mut all_roots: BTreeSet<&PathBuf> = BTreeSet::new();
    all_roots.extend(root_diag.keys());
    all_roots.extend(root_uncovered.keys());

    let mut output = String::new();

    for root in &all_roots {
        let diag_count = root_diag.get(root).map_or(0, Vec::len);
        let uncovered_count = root_uncovered.get(root).map_or(0, Vec::len);
        let total = diag_count + uncovered_count;
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
/// their original position order. Uncovered files are not budgeted — they are
/// one line each and always shown. `dirty` is `true` when any diagnostic's
/// severity meets `dirty_threshold` (LSP encoding: lower = more severe).
fn budget_diagnostics(
    diag_files: &[DiagnosticFile],
    uncovered: &[UncoveredEntry],
    budget: usize,
    dirty_threshold: u8,
) -> BudgetedDiagnostics {
    let full = format_diagnostics(diag_files, uncovered);

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
        preview: format_diagnostics(&preview_files, uncovered),
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

    // ── linter feeder translation (ticket 01) ───────────────────

    #[test]
    fn linter_diagnostic_translates_to_rendered_entry() {
        // An LSP-shaped linter diagnostic (the canonical feeder shape) renders
        // through the same formatter as LSP diagnostics, so the merged output is
        // feeder-blind: `:line:col [severity] source(code): message`.
        let diag = serde_json::json!({
            "range": {
                "start": { "line": 2, "character": 5 },
                "end": { "line": 2, "character": 8 }
            },
            "severity": 2,
            "source": "shellcheck",
            "code": "SC2086",
            "message": "Double quote to prevent globbing and word splitting."
        });
        let filter = crate::filter::get_filter("shellcheck");
        let entries = format_diagnostics_entries(
            std::slice::from_ref(&diag),
            &[],
            filter,
            "shellcheck",
            None,
            "",
            &[],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].severity, 2);
        // 0-based (2,5) renders 1-based as :3:6.
        assert!(
            entries[0]
                .text
                .contains(":3:6 [warning] shellcheck(SC2086): "),
            "unexpected render: {}",
            entries[0].text,
        );
    }

    // ── classify_file tests ─────────────────────────────────────

    #[test]
    fn classify_file_with_diagnostics() {
        let entries = vec![de(1, ":1:1 [error] test: msg")];
        assert_eq!(
            classify_file(Some(&entries)),
            FileOutcome::HasDiagnostics(vec![de(1, ":1:1 [error] test: msg")]),
        );
    }

    #[test]
    fn classify_file_empty_entries_is_clean() {
        // A feeder reached retrieval but reported clean (or every diagnostic was
        // filtered out) → present-but-empty → Clean.
        assert_eq!(classify_file(Some(&[])), FileOutcome::Clean);
    }

    #[test]
    fn classify_file_no_results() {
        assert_eq!(classify_file(None), FileOutcome::NoResults);
    }

    #[test]
    fn classify_file_merged_entries_across_feeders() {
        // After the cross-feeder pass, a file's entries are one flat list
        // (servers + linters already merged).
        let entries = vec![
            de(1, ":1:1 [error] server-a: msg"),
            de(2, ":2:1 [warning] shellcheck(SC2086): msg"),
        ];
        assert_eq!(
            classify_file(Some(&entries)),
            FileOutcome::HasDiagnostics(vec![
                de(1, ":1:1 [error] server-a: msg"),
                de(2, ":2:1 [warning] shellcheck(SC2086): msg"),
            ]),
        );
    }

    // ── format_diagnostics tests ────────────────────────────────────

    #[test]
    fn format_single_file_with_diagnostics() {
        let diag_files = vec![DiagnosticFile {
            display: "file.rs".to_string(),
            root: PathBuf::from("/test"),
            entries: vec![de(1, ":1:1 [error] test: msg")],
        }];
        let output = format_diagnostics(&diag_files, &[]);
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
        let output = format_diagnostics(&diag_files, &[]);
        // All entries should be present (no paging).
        for i in 0..5 {
            assert!(output.contains(&format!("msg {i}")), "output: {output}");
        }
    }

    #[test]
    fn format_clean_batch_is_empty() {
        // Linter idiom (misc 111): a batch with no diagnostics and no
        // uncovered files renders nothing — clean files are omitted, not
        // listed as `[clean]`.
        let output = format_diagnostics(&[], &[]);
        assert!(output.is_empty(), "expected empty output, got: {output:?}");
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
                display: "src/util.rs".to_string(),
                root: PathBuf::from("/alpha"),
                entries: vec![de(2, ":3:1 [warning] test: alpha warning")],
            },
            DiagnosticFile {
                display: "src/lib.rs".to_string(),
                root: PathBuf::from("/beta"),
                entries: vec![de(2, ":5:1 [warning] test: beta warning")],
            },
        ];
        let output = format_diagnostics(&diag_files, &[]);
        // /alpha has 2 diag files → expanded with directory header.
        let alpha_pos = output.find("/alpha\n").expect("missing /alpha header");
        assert!(output.contains("\tsrc/lib.rs:"), "output: {output}");
        assert!(output.contains("\t\t:1:1 [error]"), "output: {output}");
        assert!(output.contains("\tsrc/util.rs:"), "output: {output}");
        assert!(output.contains("alpha warning"), "output: {output}");
        // /beta has 1 file → collapsed into single path.
        let beta_pos = output
            .find("/beta/src/lib.rs:")
            .expect("missing /beta collapsed path");
        assert!(alpha_pos < beta_pos, "output: {output}");
        assert!(output.contains("beta warning"), "output: {output}");
        assert!(!output.contains("Root:"), "output: {output}");
    }

    #[test]
    fn format_clean_files_omitted_from_mixed_batch() {
        // Clean files never appear, even alongside files that have
        // diagnostics: only the dirty file is reported (misc 111).
        let diag_files = vec![DiagnosticFile {
            display: "src/lib.rs".to_string(),
            root: PathBuf::from("/alpha"),
            entries: vec![de(1, ":1:1 [error] test: alpha error")],
        }];
        let output = format_diagnostics(&diag_files, &[]);
        // Single (printed) file under /alpha → collapsed path; the clean
        // sibling produces neither a line nor an expanded directory header.
        assert!(output.contains("/alpha/src/lib.rs:"), "output: {output}");
        assert!(output.contains(":1:1 [error]"), "output: {output}");
        assert!(!output.contains("[clean]"), "output: {output}");
        assert!(!output.contains("src/main.rs"), "output: {output}");
        assert!(!output.contains("/alpha\n"), "output: {output}");
    }

    #[test]
    fn format_single_file_server() {
        let diag_files = vec![DiagnosticFile {
            display: "scratch.sh".to_string(),
            root: PathBuf::from("/tmp"),
            entries: vec![de(2, ":3:1 [warning] test: standalone warning")],
        }];
        let output = format_diagnostics(&diag_files, &[]);
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
        let output = format_diagnostics(&diag_files, &[]);
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
        let output = format_diagnostics(&[], &uncovered);
        // Single file → collapsed path with [no LSP coverage].
        assert!(output.contains("/project/data.csv\n"), "output: {output}");
        assert!(output.contains("\t[no LSP coverage]"), "output: {output}");
    }

    #[test]
    fn format_clean_omitted_uncovered_preserved() {
        // A clean covered file plus an uncovered file: the clean file is
        // omitted, the uncovered note is preserved (misc 111 keeps the
        // LSP-unavailable signal, tickets 69/80). The uncovered file is the
        // only printed entry under /project → collapsed path.
        let uncovered = vec![UncoveredEntry {
            display: "data.csv".to_string(),
            root: PathBuf::from("/project"),
        }];
        let output = format_diagnostics(&[], &uncovered);
        assert!(output.contains("/project/data.csv\n"), "output: {output}");
        assert!(output.contains("\t[no LSP coverage]"), "output: {output}");
        assert!(!output.contains("[clean]"), "output: {output}");
        assert!(!output.contains("lib.rs"), "output: {output}");
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
        let b = budget_diagnostics(&diag_files, &[], 50, 1);
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
        let b = budget_diagnostics(&diag_files, &[], 1, 1);
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
        assert!(!budget_diagnostics(&diag_files, &[], 50, 1).dirty);
        // ...but dirty when the threshold is lowered to warning.
        assert!(budget_diagnostics(&diag_files, &[], 50, 2).dirty);
    }

    // ── source-precedence reconciliation tests (misc 115, bug 42) ───

    /// Builds a diagnostic carrying a `source` and (optional) `code`.
    fn src_diag(source: &str, code: Option<&str>, msg: &str) -> Value {
        let mut d = serde_json::json!({
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            },
            "severity": 1,
            "source": source,
            "message": msg
        });
        if let Some(c) = code {
            d["code"] = serde_json::json!(c);
        }
        d
    }

    /// The rust-analyzer default chain: rustc/clippy outrank rust-analyzer,
    /// scoped to the rustc `E####` band.
    fn ra_precedence() -> crate::config::DiagnosticPrecedence {
        let mut p = crate::config::DiagnosticPrecedence {
            priority: vec![
                "rustc".to_string(),
                "clippy".to_string(),
                "rust-analyzer".to_string(),
            ],
            code_pattern: Some("^E[0-9]+$".to_string()),
            compiled_code_pattern: None,
        };
        p.compile().expect("compile code_pattern");
        p
    }

    fn source_of(d: &Value) -> &str {
        d.get("source").and_then(Value::as_str).unwrap_or("")
    }

    #[test]
    fn precedence_drops_advisory_e_code_authoritative_did_not_corroborate() {
        // Native E0107 phantom rides alongside a clean (different-error)
        // flycheck result. Once flycheck has reported for the file, the
        // native E#### is dropped — it claims a rustc error rustc didn't emit.
        let diags = vec![
            src_diag("rust-analyzer", Some("E0107"), "expected 0 args, found 1"),
            src_diag("rustc", Some("E0599"), "no method named foo"),
        ];
        let kept = reconcile_source_precedence(diags, &ra_precedence());
        // The native E0107 is gone; the authoritative rustc diagnostic stays.
        assert_eq!(kept.len(), 1, "native E#### should be dropped: {kept:?}");
        assert_eq!(source_of(&kept[0]), "rustc");
    }

    #[test]
    fn precedence_keeps_authoritative_only_e_code() {
        // A real rustc error with no native counterpart is kept untouched.
        let diags = vec![src_diag("rustc", Some("E0599"), "no method named foo")];
        let kept = reconcile_source_precedence(diags, &ra_precedence());
        assert_eq!(kept.len(), 1);
        assert_eq!(source_of(&kept[0]), "rustc");
    }

    #[test]
    fn precedence_keeps_advisory_when_no_authoritative_source_present() {
        // Single-source server case: only the advisory source reported (no
        // flycheck stream at all). The advisory E#### is NOT over-suppressed —
        // absence of an authoritative report is not contradiction.
        let diags = vec![src_diag(
            "rust-analyzer",
            Some("E0107"),
            "expected 0 args, found 1",
        )];
        let kept = reconcile_source_precedence(diags, &ra_precedence());
        assert_eq!(
            kept.len(),
            1,
            "advisory kept with no authoritative: {kept:?}"
        );
        assert_eq!(source_of(&kept[0]), "rust-analyzer");
    }

    #[test]
    fn precedence_keeps_advisory_when_authoritative_not_yet_reported_for_file() {
        // Another file's flycheck reported, but THIS file's set carries only
        // the native preview (the authoritative source has not reported here).
        // Since reconciliation runs per-file on the file's own merged set, the
        // advisory E#### is kept — absence in this set is not contradiction.
        let diags = vec![src_diag(
            "rust-analyzer",
            Some("E0107"),
            "instant pre-flycheck preview",
        )];
        let kept = reconcile_source_precedence(diags, &ra_precedence());
        assert_eq!(kept.len(), 1, "advisory kept pre-flycheck: {kept:?}");
        assert_eq!(source_of(&kept[0]), "rust-analyzer");
    }

    #[test]
    fn precedence_keeps_advisory_diagnostics_outside_the_band() {
        // Native keeps its non-rustc value even when flycheck reported: an
        // unresolved-import (string code) and an out-of-band native lint
        // survive, because they fall outside the rustc-E#### band.
        let diags = vec![
            src_diag("rust-analyzer", Some("unresolved-import"), "no such crate"),
            src_diag("rust-analyzer", None, "native lint without a code"),
            src_diag("rustc", Some("E0599"), "no method named foo"),
        ];
        let kept = reconcile_source_precedence(diags, &ra_precedence());
        // All three survive — only in-band advisory E#### codes are dropped.
        assert_eq!(kept.len(), 3, "out-of-band advisory kept: {kept:?}");
    }

    #[test]
    fn precedence_without_code_pattern_drops_all_advisory_once_authoritative_reports() {
        // No code_pattern → the whole-diagnostic set is the band. Every
        // lower-priority diagnostic is dropped once a higher-ranked one is present.
        let mut p = crate::config::DiagnosticPrecedence {
            priority: vec!["syntactic".to_string(), "semantic".to_string()],
            code_pattern: None,
            compiled_code_pattern: None,
        };
        p.compile().expect("compile");
        let diags = vec![
            src_diag("semantic", Some("anything"), "advisory"),
            src_diag("syntactic", None, "authoritative"),
        ];
        let kept = reconcile_source_precedence(diags, &p);
        assert_eq!(kept.len(), 1);
        assert_eq!(source_of(&kept[0]), "syntactic");
    }

    // ── cross-feeder dedup + precedence tests (ticket 02) ───────────

    /// Builds a diagnostic at an explicit position carrying a `source` and
    /// (optional) `code`.
    fn diag_at(source: &str, code: Option<&str>, line: u32, col: u32, msg: &str) -> Value {
        let mut d = serde_json::json!({
            "range": {
                "start": { "line": line, "character": col },
                "end": { "line": line, "character": col + 1 }
            },
            "severity": 2,
            "source": source,
            "message": msg
        });
        if let Some(c) = code {
            d["code"] = serde_json::json!(c);
        }
        d
    }

    /// Wraps a diagnostic value into a [`FeederEntry`] with a feeder context
    /// keyed by `command` (so two feeders can be distinguished, though dedup is
    /// feeder-blind).
    fn fe(command: &str, value: Value) -> FeederEntry {
        FeederEntry {
            value,
            fixes: Vec::new(),
            enclosing: None,
            ctx: Arc::new(FeederContext {
                command: command.to_string(),
                version: None,
                language_id: String::new(),
            }),
        }
    }

    fn entry_source(e: &FeederEntry) -> &str {
        e.value.get("source").and_then(Value::as_str).unwrap_or("")
    }

    #[test]
    fn dedup_collapses_same_finding_across_feeders() {
        // bash-language-server (an LSP feeder) and standalone shellcheck both
        // report SC2086 at the same line: same (source, code, line) → one entry.
        let entries = vec![
            fe(
                "bash-language-server",
                diag_at(
                    "shellcheck",
                    Some("SC2086"),
                    4,
                    2,
                    "Double quote to prevent globbing",
                ),
            ),
            fe(
                "shellcheck",
                diag_at(
                    "shellcheck",
                    Some("SC2086"),
                    4,
                    9,
                    "Double quote to prevent globbing.",
                ),
            ),
        ];
        let deduped = dedupe_entries(entries);
        assert_eq!(deduped.len(), 1, "the wrapped + standalone copy collapse");
        // First occurrence (the LSP feeder) wins.
        assert_eq!(deduped[0].ctx.command, "bash-language-server");
    }

    #[test]
    fn dedup_anchors_on_line_not_column() {
        // Same source/code/line, drifting columns (LSP 0-based vs CLI 1-based)
        // collapse — the key is line-anchored, bias coarse.
        let entries = vec![
            fe("a", diag_at("sc", Some("SC1000"), 7, 0, "msg")),
            fe("b", diag_at("sc", Some("SC1000"), 7, 40, "msg")),
        ];
        assert_eq!(dedupe_entries(entries).len(), 1);
    }

    #[test]
    fn dedup_keeps_distinct_line_source_and_code() {
        // Different line, different source, and different code each stay.
        let entries = vec![
            fe("a", diag_at("sc", Some("SC1000"), 7, 0, "msg")),
            fe("a", diag_at("sc", Some("SC1000"), 8, 0, "msg")), // different line
            fe("a", diag_at("other", Some("SC1000"), 7, 0, "msg")), // different source
            fe("a", diag_at("sc", Some("SC1001"), 7, 0, "msg")), // different code
        ];
        assert_eq!(dedupe_entries(entries).len(), 4, "nothing collapses");
    }

    #[test]
    fn dedup_codeless_fallback_keys_on_normalized_message() {
        // No code → fall back to (source, normalized-message, line). Whitespace
        // and case differences in the message still collapse; a genuinely
        // different message does not.
        let entries = vec![
            fe("a", diag_at("yaml", None, 3, 0, "trailing   spaces")),
            fe("b", diag_at("yaml", None, 3, 4, "Trailing spaces")), // normalizes equal
            fe("c", diag_at("yaml", None, 3, 0, "wrong indentation")), // distinct
        ];
        let deduped = dedupe_entries(entries);
        assert_eq!(
            deduped.len(),
            2,
            "normalized duplicate collapses, distinct stays"
        );
    }

    #[test]
    fn dedup_codeless_does_not_collide_with_coded() {
        // A codeless entry whose message text equals another entry's code must
        // not collapse into it — the NUL-tagged discriminant separates them.
        let entries = vec![
            fe("a", diag_at("x", Some("SC2086"), 1, 0, "real message")),
            fe("b", diag_at("x", None, 1, 0, "SC2086")),
        ];
        assert_eq!(dedupe_entries(entries).len(), 2);
    }

    #[test]
    fn reconcile_entries_drops_advisory_across_merged_feeders() {
        // The advisory native E#### and the authoritative rustc E#### arrive as
        // a single merged cross-feeder set; reconciliation drops the in-band
        // advisory once authoritative has reported.
        let entries = vec![
            fe(
                "rust-analyzer",
                diag_at("rust-analyzer", Some("E0107"), 0, 0, "phantom"),
            ),
            fe(
                "rust-analyzer",
                diag_at("rustc", Some("E0599"), 1, 0, "no method foo"),
            ),
        ];
        let kept = reconcile_entries(entries, &[ra_precedence()]);
        assert_eq!(kept.len(), 1, "advisory in-band E#### dropped: {kept:?}");
        assert_eq!(entry_source(&kept[0]), "rustc");
    }

    #[test]
    fn reconcile_entries_keeps_all_when_no_policy() {
        // Empty policy list (the `[diagnostics] precedence = []` opt-out) → union.
        let entries = vec![
            fe(
                "rust-analyzer",
                diag_at("rust-analyzer", Some("E0107"), 0, 0, "phantom"),
            ),
            fe(
                "rust-analyzer",
                diag_at("rustc", Some("E0599"), 1, 0, "no method foo"),
            ),
        ];
        let kept = reconcile_entries(entries, &[]);
        assert_eq!(kept.len(), 2, "no policy keeps the union");
    }

    #[test]
    fn reconcile_entries_keeps_advisory_when_no_authoritative() {
        // Advisory-only merged set → kept (absence of authoritative is not
        // contradiction), even with the policy active.
        let entries = vec![fe(
            "rust-analyzer",
            diag_at("rust-analyzer", Some("E0107"), 0, 0, "preview"),
        )];
        let kept = reconcile_entries(entries, &[ra_precedence()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(entry_source(&kept[0]), "rust-analyzer");
    }
}
