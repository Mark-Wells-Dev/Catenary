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
/// errors-before-warnings ordering, and the clean/dirty status
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

/// Outcome of a `catenary diagnostics` run: the per-file receipt text plus the
/// clean/dirty status label.
pub struct DiagnosticsOutcome {
    /// The complete per-file receipt for stdout (decision 025) — every
    /// diagnosed file, `[clean]` beside the clean ones, diagnostics beneath the
    /// dirty ones, and an `[unverified — <server> returned no result]` line for
    /// any file whose server produced nothing (bug 56); no volume branch.
    pub output: String,
    /// `true` when at least one diagnostic met the dirty severity threshold.
    /// A status label only (workstream 37 ticket 01): the run exits `0`
    /// whether clean or dirty — the clean/dirty distinction lives in the
    /// receipt, not the exit code.
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

/// A named path a scoped pull could not scope: it does not exist, or it
/// resolves outside every mounted root.
///
/// A scoped `catenary diagnostics <path>` names paths directly, so each named
/// path is a direct request that earns an explicit receipt line — never the
/// silent drop that used to render empty stdout (bug 58 / ephemeral-roots
/// ticket 01). These entries carry no mounted root to group under, so they
/// render as flat one-line receipts appended to the body.
struct OutOfScopeEntry {
    /// The named path as resolved (home-compressed), for display.
    display: String,
    /// Why the path could not be scoped.
    kind: OutOfScopeKind,
}

/// The reason a named path fell out of scope, driving its receipt wording.
enum OutOfScopeKind {
    /// The named path does not exist on disk (it could not be canonicalized).
    Missing,
    /// The named path exists but resolves outside every mounted root. Carries
    /// the enclosing project root when one is detectable (walk repository markers up from
    /// the path) — what a `catenary pin` would mount — or `None` when no
    /// enclosing project root is found.
    OutsideRoots { enclosing_root: Option<PathBuf> },
}

/// A covered file that a feeder verified with no diagnostics.
///
/// Listed in the receipt as `[clean]` beside its path — the explicit
/// counterpart to a dirty file's diagnostics (workstream 37 ticket 01,
/// retiring silent-on-clean / decision 022 / misc 111). Carries the same
/// display + grouping-root shape as [`UncoveredEntry`].
struct CleanEntry {
    display: String,
    /// Grouping root: workspace root, or parent directory for
    /// files outside all roots.
    root: PathBuf,
}

/// A covered file whose server produced no result — the server died mid-pipeline
/// or never answered, so the file is neither `[clean]` nor diagnosed.
///
/// Listed in the receipt as an explicit `[unverified — <server> returned no
/// result]` line beside its path (bug 56): the deliberate ticket-01 rule that
/// [`FileOutcome::NoResults`] earns no `[clean]` line is right, but an
/// all-`NoResults` set used to omit every file and render as empty stdout —
/// indistinguishable from a hang or silent failure, while still paying the debt.
/// Surfacing the unverified remainder keeps the receipt honest. Carries the same
/// display + grouping-root shape as [`CleanEntry`], plus the name of the
/// server(s) that failed to answer. Debt/gate semantics are unchanged — this is
/// a render-only line.
struct UnverifiedEntry {
    display: String,
    /// Grouping root: workspace root, or parent directory for
    /// files outside all roots.
    root: PathBuf,
    /// The diagnostic server(s) assigned to the file that produced no result,
    /// joined for display (e.g. `rust-analyzer`).
    server: String,
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

/// The routing decision for a scoped diagnostics request (workstream 37
/// ticket 04, decision 5).
///
/// Produced by [`DiagnosticsServer::plan_scope`]: the requested paths split into
/// a per-file fan-out set and a set of whole-root workspace-pull targets, plus
/// the `directory_scoped` flag that drives the clean-collapse render rule.
#[derive(Default)]
struct ScopePlan {
    /// Files for the per-file fan-out lifecycle: explicit file arguments plus
    /// the expanded contents of sub-root / no-capability directories.
    fan_out: Vec<PathBuf>,
    /// Whole tracked roots to serve via one `workspace/diagnostic` request each,
    /// paired with the capable clients covering the root.
    workspace_targets: Vec<(PathBuf, Vec<Arc<Mutex<LspClient>>>)>,
    /// Set when any requested path is a directory (`.`, a whole root, or a
    /// sub-root). Triggers the clean-collapse receipt rule; a bare file set (the
    /// edit-loop receipt) leaves it `false` and renders per-file.
    directory_scoped: bool,
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

    /// Processes a scoped diagnostics request, routing by capability and scope
    /// (workstream 37 ticket 04) and rendering one unified receipt.
    ///
    /// The requested paths are split ([`Self::plan_scope`]) into two mechanisms:
    ///
    /// - **whole tracked root + a `workspace/diagnostic`-capable server** → one
    ///   [`Self::workspace_pull`] off the server's existing project model (no
    ///   per-file `didOpen`/`didClose` churn; cross-file diagnostics surfaced);
    /// - **everything else** (explicit files, sub-root directories, roots with no
    ///   capable server) → the per-file [`Self::fan_out`] lifecycle (ticket 02's
    ///   path, the always-available fallback).
    ///
    /// Both mechanisms feed one per-file map, so the aggregation, classification,
    /// and receipt render once. A directory/whole-root scope collapses the
    /// `[clean]` list to a count (`plan.directory_scoped`); the edit-loop receipt
    /// (a file set) stays per-file exactly as ticket 01 shipped it.
    pub async fn process_files_batched(
        &self,
        files: &[PathBuf],
        parent_id: Option<&str>,
    ) -> DiagnosticsOutcome {
        if files.is_empty() {
            return DiagnosticsOutcome {
                output: String::new(),
                dirty: false,
                errors: 0,
                warnings: 0,
            };
        }

        // ── Phase 0: scope routing ─────────────────────────────────
        let plan = self.plan_scope(files).await;

        let mut feeds: BTreeMap<String, FileFeed> = BTreeMap::new();

        // Fan-out engine (ticket 02's path): explicit files plus sub-root /
        // no-capability directories expanded to their covered files. Also yields
        // the per-file server assignment, so a file that dies before producing a
        // result can name the server(s) that owed it one (bug 56).
        let (mut canonical_paths, uncovered, out_of_scope, path_servers) =
            self.fan_out(&plan.fan_out, parent_id, &mut feeds).await;

        // Whole-root + capable scopes: one workspace/diagnostic request each,
        // merged into the same per-file map the fan-out populated.
        for (root, clients) in &plan.workspace_targets {
            self.workspace_pull(root, clients, &mut feeds, &mut canonical_paths)
                .await;
        }

        // ── Phase 2c: cross-feeder aggregation (ticket 02) ─────────
        let file_results = self.aggregate_feeds(feeds);

        // Mirror decision-027 coverage degradation onto the server board: a
        // server that owed a file a result and produced none has degraded, one
        // that produced has recovered. Today this state lives only in the
        // receipt banner; record it where the health surface reads it too.
        self.record_diag_degradation(&canonical_paths, &file_results, &path_servers);

        // ── Phase 3: classify and format ─────────────────────────
        let outcome = self.format_output(
            &canonical_paths,
            &file_results,
            &uncovered,
            &out_of_scope,
            &path_servers,
            plan.directory_scoped,
        );

        // ── Phase 4: invalidate caches ────────────────────────────
        self.fs.bump_generations(&canonical_paths);

        outcome
    }

    /// The per-file fan-out lifecycle: the always-available diagnostics engine
    /// (ticket 02's path), now factored behind the ticket-04 scope router.
    ///
    /// Pipeline: resolve + canonicalize → group by server → per server (open all
    /// → settle → health probe → didSave all → settle → retrieve per file → close
    /// all) → linter feeders. Populates `feeds` with each file's raw feeder
    /// diagnostics and returns the covered `canonical_paths`, the uncovered list,
    /// the out-of-scope list (named paths that do not exist or resolve outside
    /// every mounted root — bug 58 / ephemeral-roots ticket 01), and a per-file
    /// map of the diagnostic server(s) assigned to each covered file (keyed by
    /// canonical-path string). The last is how a file that never produced a
    /// result — because its server died mid-pipeline — can still name the server
    /// that owed it one in the unverified receipt line (bug 56).
    ///
    /// Cross-file diagnostics (e.g., a renamed type that breaks importers) are
    /// correct because every server sees the complete final state before
    /// producing diagnostics.
    #[allow(
        clippy::type_complexity,
        clippy::too_many_lines,
        reason = "batch pipeline: the server-grouping map is local; the phased lifecycle (resolve / nudge / LSP / linter) reads top-to-bottom"
    )]
    async fn fan_out(
        &self,
        files: &[PathBuf],
        parent_id: Option<&str>,
        feeds: &mut BTreeMap<String, FileFeed>,
    ) -> (
        Vec<PathBuf>,
        Vec<UncoveredEntry>,
        Vec<OutOfScopeEntry>,
        BTreeMap<String, BTreeSet<String>>,
    ) {
        if files.is_empty() {
            return (Vec::new(), Vec::new(), Vec::new(), BTreeMap::new());
        }

        // Ensure servers exist for all files before looking them up.
        // Triggers lazy spawn for files in sub-roots that haven't
        // been visited by grep/glob yet (root marker resolution).
        self.client_manager.ensure_clients_for_paths(files).await;

        // ── Phase 1: resolve + canonicalize ────────────────────────
        let mut canonical_paths: Vec<PathBuf> = Vec::new();
        let mut uncovered: Vec<UncoveredEntry> = Vec::new();
        // Named paths that could not be scoped: nonexistent, or outside every
        // mounted root. Kept explicit so a scoped pull never renders empty
        // stdout for a named path (bug 58 / ephemeral-roots ticket 01).
        let mut out_of_scope: Vec<OutOfScopeEntry> = Vec::new();
        // The lint-covered subset, fanned out to standalone linters in Phase 2b
        // (workstream 34 ticket 01). A file can be both LSP- and lint-covered.
        let mut lint_candidates: Vec<PathBuf> = Vec::new();

        // Server → list of canonical paths.
        // Keyed by server name for stable (alphabetical) iteration order.
        let mut server_groups: BTreeMap<String, (Arc<Mutex<LspClient>>, Vec<PathBuf>)> =
            BTreeMap::new();

        // Canonical-path string → the diagnostic server(s) assigned to it. Lets a
        // file that resolves `NoResults` (its server died before producing) name
        // the responsible server in the unverified receipt line (bug 56).
        let mut path_servers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        let validator = self.path_validator.read().await;
        for file in files {
            let file_str = file.to_string_lossy();

            // Resolve to absolute if needed (the editing-manager drain
            // already returns absolute paths, but be defensive). A path that
            // cannot be made absolute (a relative path with no resolvable cwd)
            // is treated as nonexistent rather than dropped silently — a named
            // path is a direct request and earns a line (bug 58).
            let Ok(path) = resolve_path(&file_str) else {
                out_of_scope.push(OutOfScopeEntry {
                    display: super::compress_home(std::path::Path::new(file_str.as_ref())),
                    kind: OutOfScopeKind::Missing,
                });
                continue;
            };

            // The LSP-awareness gate declined the path: it is either nonexistent
            // or outside every mounted root. Classify it so the receipt says
            // which — instead of the silent drop that rendered empty stdout for
            // a named out-of-root/nonexistent path (bug 58 / ticket 01).
            let Ok(canonical) = validator.validate_read(&path) else {
                out_of_scope.push(classify_out_of_scope(&path));
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
                    // diagnostics render in the format pass. Effective coverage:
                    // a live linter covers the file even when the LSP server is
                    // down, so this takes precedence over the degraded branch.
                    canonical_paths.push(canonical.clone());
                    lint_candidates.push(canonical);
                    continue;
                }
                // A configured diagnostic server that cannot start (the
                // spawn-failure / dies-at-`initialize` class) leaves a dead
                // tombstone that `diagnostic_servers` filters out. Decision 027:
                // that is a coverage *degradation*, not an absence — route the
                // file to the degraded/unverified path (naming the dead
                // server), never a bare `[no LSP coverage]`, so it earns the
                // receipt's `unavailable:` banner and an `[unverified — …]`
                // line, the same treatment a mid-run death gets. No live server
                // means no batch runs for it, so it resolves `NoResults`.
                let dead_servers = self
                    .client_manager
                    .unavailable_diagnostic_servers(&canonical)
                    .await;
                if dead_servers.is_empty() {
                    let display = self.display_rel(&canonical.to_string_lossy());
                    let root = self.resolve_root_or_parent(&canonical);
                    uncovered.push(UncoveredEntry { display, root });
                } else {
                    let key = canonical.to_string_lossy().to_string();
                    let entry = path_servers.entry(key).or_default();
                    for name in dead_servers {
                        entry.insert(name);
                    }
                    canonical_paths.push(canonical);
                }
                continue;
            }

            canonical_paths.push(canonical.clone());
            if lint_covered {
                lint_candidates.push(canonical.clone());
            }

            let key = canonical.to_string_lossy().to_string();
            for client_mutex in &clients {
                let name = client_mutex.lock().await.server_name().to_string();
                path_servers
                    .entry(key.clone())
                    .or_default()
                    .insert(name.clone());
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
        for (client_mutex, paths) in server_groups.values() {
            self.run_server_batch_with_recovery(client_mutex, paths, parent_id, feeds)
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
                let key = feed.file.to_string_lossy().to_string();
                let display = self.display_rel(&key);
                // Record the file even when the linter ran and found nothing, so
                // it classifies Clean, not NoResults — an empty lint result is a
                // verification, not an absence (bug 56 ruling 2 / ticket 06),
                // mirroring retrieve_diagnostics' record-even-with-zero rule. A
                // linter that never completed (not installed, spawn/parse failure)
                // yields no feed for the file, so it stays unverified.
                let file_feed = feeds.entry(key).or_insert_with(|| FileFeed {
                    display,
                    entries: Vec::new(),
                });
                if feed.diagnostics.is_empty() {
                    continue;
                }
                let ctx = Arc::new(FeederContext {
                    command: feed.command,
                    version: None,
                    language_id: String::new(),
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

        (canonical_paths, uncovered, out_of_scope, path_servers)
    }

    /// Routes a scoped diagnostics request across the fan-out and workspace-pull
    /// mechanisms (workstream 37 ticket 04, decision 5).
    ///
    /// Each requested path is classified:
    ///
    /// - a **file** (or any non-directory) → fan-out;
    /// - a **directory that is a whole tracked root** *and* has a live,
    ///   `workspace/diagnostic`-capable server → a workspace-pull target;
    /// - any **other directory** (a sub-root, or a root whose server lacks the
    ///   capability / is not yet spawned) → expanded to its covered files and
    ///   fanned out.
    ///
    /// `directory_scoped` is set whenever any requested path is a directory — the
    /// signal for the clean-collapse render rule, independent of which mechanism
    /// serves it.
    async fn plan_scope(&self, files: &[PathBuf]) -> ScopePlan {
        let mut plan = ScopePlan::default();
        let tracked_roots = self.fs.roots();

        for file in files {
            let Ok(abs) = resolve_path(&file.to_string_lossy()) else {
                plan.fan_out.push(file.clone());
                continue;
            };
            if !abs.is_dir() {
                plan.fan_out.push(abs);
                continue;
            }

            // A directory scope: `.`, a whole root, or a sub-root directory.
            plan.directory_scoped = true;

            // Whole tracked root? Compare canonically so a symlinked or
            // non-canonical `.` still matches the registered root value.
            let matched_root = tracked_roots
                .iter()
                .find(|root| same_path(root.as_path(), &abs));
            if let Some(root) = matched_root {
                // `disable_diag` roots surface nothing — neither mechanism.
                if self.client_manager.is_diag_disabled(root) {
                    continue;
                }
                let clients = self.client_manager.workspace_diagnostic_clients(root).await;
                if !clients.is_empty() {
                    plan.workspace_targets.push((root.clone(), clients));
                    continue;
                }
            }

            // Sub-root directory, or a root with no capable/live server: expand
            // to the covered files and fan them out (the fallback).
            plan.fan_out.extend(self.expand_directory(&abs));
        }

        plan
    }

    /// Walks a directory (gitignore-aware, hidden-skipping) and returns the
    /// files a diagnostic feeder covers.
    ///
    /// Only files whose language resolves to a configured server binding or that
    /// a standalone linter covers are kept — a whole-directory scope diagnoses
    /// the diagnosable files, it does not flag every unrelated file as
    /// `[no LSP coverage]`. Shares the walk scope of [`stat_walk`] and the grep
    /// walker (respects `.gitignore`, skips hidden entries).
    fn expand_directory(&self, dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let walker = WalkBuilder::new(dir).git_ignore(true).hidden(true).build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path().to_path_buf();
            // Detect language the same way `get_servers` does: the filesystem
            // classifier first, then the raw extension (a synthesized/config
            // language whose extension the classifier doesn't index still routes
            // by its extension).
            let lang = self.fs.language_id(&path).or_else(|| {
                path.extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_string)
            });
            let lsp_covered = lang.is_some_and(|lang| {
                // Resolve per-root so a project `[lsp.language.*]` binding counts
                // as coverage — mirrors the dispatch sites (misc 155). Unrooted
                // files fall back to the global resolution.
                self.fs.resolve_root(&path).map_or_else(
                    || {
                        self.client_manager
                            .config()
                            .resolve_language(&lang)
                            .is_some()
                    },
                    |root| {
                        self.client_manager
                            .effective_language(&root, &lang)
                            .is_some()
                    },
                )
            });
            if lsp_covered || self.client_manager.lint_covers(&path) {
                files.push(path);
            }
        }
        files
    }

    /// Issues one `workspace/diagnostic` request per capable server for `root`
    /// and merges the per-file reports into the shared `feeds` map.
    ///
    /// Off the server's existing project model — no per-file `didOpen`/`didClose`
    /// churn — so quick-fix code actions and enclosing-symbol labels (both of
    /// which need an open document) are deliberately skipped on this path; the
    /// cross-file diagnostics the per-file pull can miss are the payoff. Every
    /// reported document (clean ones included) is recorded in `feeds` and
    /// `canonical_paths` so the receipt classifies and collapses it. The
    /// server's own project scope bounds the report to `root`; a stray report
    /// outside `root` is dropped defensively.
    async fn workspace_pull(
        &self,
        root: &std::path::Path,
        clients: &[Arc<Mutex<LspClient>>],
        feeds: &mut BTreeMap<String, FileFeed>,
        canonical_paths: &mut Vec<PathBuf>,
    ) {
        for client_mutex in clients {
            let client = client_mutex.lock().await;
            let server_name = client.server_name().to_string();
            let ctx = Arc::new(FeederContext {
                command: client.server_command().to_string(),
                version: client.server_version().map(str::to_string),
                language_id: client.language().to_string(),
            });
            let reports = match client.workspace_diagnostics().await {
                Ok(reports) => reports,
                Err(e) => {
                    debug!(
                        server = %server_name,
                        "workspace/diagnostic failed, skipping: {e}",
                    );
                    continue;
                }
            };
            drop(client);

            for (uri, diagnostics) in reports {
                let Some(path) = uri_to_pathbuf(&uri) else {
                    continue;
                };
                if !path.starts_with(root) {
                    continue;
                }
                let diagnostics = self.filter_min_severity(&server_name, diagnostics);

                let key = path.to_string_lossy().to_string();
                let display = self.display_rel(&key);
                let file_feed = feeds.entry(key).or_insert_with(|| FileFeed {
                    display,
                    entries: Vec::new(),
                });
                for value in diagnostics {
                    file_feed.entries.push(FeederEntry {
                        value,
                        fixes: Vec::new(),
                        enclosing: None,
                        ctx: Arc::clone(&ctx),
                    });
                }
                if !canonical_paths.contains(&path) {
                    canonical_paths.push(path);
                }
            }
        }
    }

    /// Applies a server's configured `min_severity` floor to a diagnostic set.
    ///
    /// Mirrors the per-file pull's pre-render filter so the workspace-pull path
    /// honours the same `[server.*].min_severity` policy. A diagnostic with no
    /// severity is kept (it never gates).
    fn filter_min_severity(&self, server_name: &str, diagnostics: Vec<Value>) -> Vec<Value> {
        let min_severity = self
            .client_manager
            .config()
            .server
            .get(server_name)
            .and_then(|sd| sd.min_severity.as_deref())
            .and_then(crate::filter::parse_severity);
        match min_severity {
            Some(threshold) => diagnostics
                .into_iter()
                .filter(|d| {
                    crate::lsp::extract::diagnostic_severity(d)
                        .is_none_or(|sev| crate::filter::severity_passes(sev, threshold))
                })
                .collect(),
            None => diagnostics,
        }
    }

    /// Cross-feeder aggregation: per file, dedup → provisional drop → render
    /// (workstream 34 ticket 05).
    ///
    /// Runs over the merged raw diagnostics each file accumulated from every
    /// feeder (language servers and linters). Reconciliation is **union →
    /// cross-source dedup (heaviest-weight keeper) → provisional drop**, keyed on
    /// the per-root effective [`DiagnosticWeights`](crate::config::DiagnosticWeights).
    /// Dedup collapses the same finding across sources; the provisional pass drops
    /// only a flycheck-contradicted phantom. Both operate on canonical
    /// LSP-diagnostic JSON, so the pass is feeder-blind. Each surviving entry is
    /// then rendered through its own feeder's context; the per-key presence (even
    /// with zero rendered entries) is preserved so the downstream
    /// clean-vs-no-results distinction survives.
    ///
    /// Weights are resolved once per distinct root in the batch (memoized), since
    /// resolving compiles the provisional bands.
    fn aggregate_feeds(
        &self,
        feeds: BTreeMap<String, FileFeed>,
    ) -> BTreeMap<String, (String, Vec<DiagEntry>)> {
        let mut rendered: BTreeMap<String, (String, Vec<DiagEntry>)> = BTreeMap::new();
        let mut weight_cache: std::collections::HashMap<
            Option<PathBuf>,
            crate::config::DiagnosticWeights,
        > = std::collections::HashMap::new();
        for (key, feed) in feeds {
            let path = PathBuf::from(&key);
            let root = self.fs.resolve_root(&path);
            let weights = weight_cache
                .entry(root.clone())
                .or_insert_with(|| self.client_manager.effective_weights(root.as_deref()));

            let deduped = dedupe_entries(feed.entries, weights);
            let reconciled = drop_challenged_provisional(deduped, weights);

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

    /// Classifies files from server results, renders the per-file receipt, and
    /// reports the clean/dirty status.
    ///
    /// Root-grouped file entries: dirty files list their diagnostics beneath,
    /// clean files carry a `[clean]` line beside their path, unverified files an
    /// `[unverified — <server> returned no result]` line, uncovered files a
    /// `[no LSP coverage]` note. Clean is **explicit**, never silence
    /// (workstream 37 ticket 01, retiring misc 111 / decision 022): the receipt
    /// is proof of the debt paid, so every diagnosed file appears. Root headers
    /// are collapsed when only one printed file exists under that root. The
    /// report is always complete (decision 025) — every diagnostic prints, with
    /// no volume branch.
    ///
    /// `path_servers` maps each covered file to the diagnostic server(s) assigned
    /// to it, so a [`FileOutcome::NoResults`] file (its server died before
    /// producing) can name the responsible server in its unverified line (bug 56).
    ///
    /// `collapse_clean` (a directory/whole-root scope, ticket 04) folds each
    /// root's `[clean]` list to a single `N files clean` line — and, likewise,
    /// its unverified list to `M files unverified` — since the dirty files are
    /// the signal. The diagnostics themselves are never collapsed (decision 025);
    /// only the clean and unverified lists are.
    ///
    /// `out_of_scope` carries the named paths that could not be scoped
    /// (nonexistent, or outside every mounted root); each renders one explicit
    /// line appended to the body so a scoped pull never renders empty stdout for
    /// a named path (bug 58 / ephemeral-roots ticket 01).
    fn format_output(
        &self,
        canonical_paths: &[PathBuf],
        file_results: &BTreeMap<String, (String, Vec<DiagEntry>)>,
        uncovered: &[UncoveredEntry],
        out_of_scope: &[OutOfScopeEntry],
        path_servers: &BTreeMap<String, BTreeSet<String>>,
        collapse_clean: bool,
    ) -> DiagnosticsOutcome {
        let mut diag_files: Vec<DiagnosticFile> = Vec::new();
        let mut clean_files: Vec<CleanEntry> = Vec::new();
        let mut unverified_files: Vec<UnverifiedEntry> = Vec::new();

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
                // A feeder reached retrieval and reported no diagnostics: the
                // file is verified clean, so it earns an explicit `[clean]`
                // line in the receipt (no longer silent).
                FileOutcome::Clean => {
                    clean_files.push(CleanEntry { display, root });
                }
                // No feeder produced a result (the server died mid-pipeline or
                // never answered): the file was NOT verified, so it earns neither
                // a `[clean]` line nor diagnostics. It is still surfaced as an
                // explicit `[unverified — <server> returned no result]` line
                // (bug 56) — naming the server(s) that owed it a result — so an
                // all-`NoResults` set can never render as empty stdout,
                // indistinguishable from a hang. Debt/gate semantics are
                // unchanged: the drain still clears it. Only a server-assigned
                // file names a server here; a lint-only file that produced
                // nothing carries no server, so it stays out of the receipt as
                // before.
                FileOutcome::NoResults => {
                    if let Some(servers) = path_servers.get(&key) {
                        let server = servers
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(", ");
                        unverified_files.push(UnverifiedEntry {
                            display,
                            root,
                            server,
                        });
                    }
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
        // dirty threshold (error) always applies.
        let dirty_threshold = self
            .client_manager
            .config()
            .tools
            .clone()
            .unwrap_or_default()
            .dirty_severity();
        let dirty = diag_files
            .iter()
            .flat_map(|f| &f.entries)
            .any(|e| crate::filter::severity_passes(e.severity, dirty_threshold));

        // The report is always complete (decision 025): render every diagnostic
        // inline — no budget, no spill, no pointer line. A directory/whole-root
        // scope collapses the `[clean]` and unverified lists to counts (ticket 04
        // / bug 56).
        let mut output = format_diagnostics(
            &diag_files,
            uncovered,
            &clean_files,
            &unverified_files,
            collapse_clean,
        );

        // A scoped pull's named paths that fell out of scope (nonexistent, or
        // outside every mounted root) render as flat one-line receipts appended
        // to the body — after the root-grouped sections and the unavailable
        // banner, so the change composes with the banner rather than displacing
        // it (bug 58 / ephemeral-roots ticket 01).
        output.push_str(&render_out_of_scope(out_of_scope));

        DiagnosticsOutcome {
            output,
            dirty,
            errors,
            warnings,
        }
    }

    /// Runs a server's batch, then makes one bounded recovery attempt if the
    /// server died mid-batch (decision 027 — the gate may stall, never wedge).
    ///
    /// A file is present in `feeds` once any feeder recorded it (even clean);
    /// a file assigned to this server but absent from `feeds` after the batch
    /// is the documented [`FileOutcome::NoResults`] producer — the server
    /// bailed before recording it. When that remainder is non-empty **and the
    /// server died** (a terminal lifecycle — [`await_idle`](crate::lsp::settle)
    /// sets it on root death, so this is a deterministic signal, never a
    /// wall-clock probe), one respawn is attempted and the unretrieved
    /// remainder is re-run against the fresh instance, bounded by the same
    /// spawn/initialize budgets. If the respawn fails or dies again, the
    /// still-unretrieved files stay `NoResults` and degrade through the receipt
    /// (`unavailable:` banner + `[unverified — …]` lines).
    ///
    /// An alive, non-terminal server that merely failed to open a file is left
    /// as-is — that is not an unavailability, and a respawn would not change it.
    async fn run_server_batch_with_recovery(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        parent_id: Option<&str>,
        feeds: &mut BTreeMap<String, FileFeed>,
    ) {
        self.run_server_batch(client_mutex, paths, parent_id, feeds)
            .await;

        let remainder: Vec<PathBuf> = paths
            .iter()
            .filter(|p| !feeds.contains_key(p.to_string_lossy().as_ref()))
            .cloned()
            .collect();
        if remainder.is_empty() {
            return;
        }

        let key = {
            let client = client_mutex.lock().await;
            // Recover only from an actual death; an alive, non-terminal server
            // is not an unavailability.
            if client.is_alive() && !client.lifecycle().is_terminal() {
                return;
            }
            client.server().key()
        };
        let Some(key) = key else {
            return;
        };

        // One bounded respawn + re-run of the remainder. Any file still
        // unretrieved afterwards stays `NoResults` (degrade — no further
        // attempts this run).
        if let Some(fresh) = self.client_manager.respawn_diagnostic_server(&key).await {
            self.run_server_batch(&fresh, &remainder, parent_id, feeds)
                .await;
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
            let diagnostics = match client.get_diagnostics(uri) {
                // A publish was heard for this file — authoritative, EVEN WHEN
                // EMPTY. `Some(vec![])` is the server's evidence-backed clean: an
                // explicit empty publish, not the absence of one. Pull is never
                // consulted, so a push+pull server reports one source per file
                // per run (no double-reporting) and a push-only server's honest
                // empty is never second-guessed by an off-spec probe (misc 153).
                Some(diags) => diags,
                // Never heard AND the server advertises the pull model: ask it
                // the way it asked to be asked. Unchanged.
                None if client.supports_pull_diagnostics() => {
                    match client.pull_diagnostics(uri).await {
                        Ok(diags) => diags,
                        Err(e) => {
                            client.server().downgrade_pull_diagnostics();
                            debug!("pull diagnostics failed, downgraded: {e}");
                            Vec::new()
                        }
                    }
                }
                // Never heard and no advertised pull capability. Some servers
                // still answer `textDocument/diagnostic` on demand without
                // advertising it (lattice): ask directly rather than report a
                // false `[clean]` for a fast publisher whose first publish the
                // settle-then-collect pipeline cleared (bug 74). Errors map to
                // empty, so a genuinely silent push-only server is unchanged.
                None => client.try_pull_diagnostics(uri).await,
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

    /// Records decision-027 coverage degradation on the server board.
    ///
    /// Keyed exactly as the receipt banner ([`prepend_unavailable_banner`]): a
    /// per-root instance is *degraded* this run iff it was assigned a file that
    /// produced no result (absent from `file_results` — it died mid-run or
    /// failed to start), and *recovered* iff it produced a result and degraded
    /// nothing. The degraded ids gain `degraded_since`/`degraded_reason`; the
    /// recovered ids are cleared — so the state that until now lived only in the
    /// banner also rides the snapshot the health surface reads. No-op without a
    /// wired snapshot (doctor / tests).
    fn record_diag_degradation(
        &self,
        canonical_paths: &[PathBuf],
        file_results: &BTreeMap<String, (String, Vec<DiagEntry>)>,
        path_servers: &BTreeMap<String, BTreeSet<String>>,
    ) {
        let Some(writer) = self.client_manager.snapshot() else {
            return;
        };
        // Per-root instance ids the run degraded vs. verified. A missing
        // `file_results` entry is the `FileOutcome::NoResults` producer.
        let mut degraded: BTreeSet<String> = BTreeSet::new();
        let mut recovered: BTreeSet<String> = BTreeSet::new();
        for cp in canonical_paths {
            let key = cp.to_string_lossy().to_string();
            let Some(servers) = path_servers.get(&key) else {
                continue;
            };
            let root = self.resolve_root_or_parent(cp);
            let target = if file_results.contains_key(&key) {
                &mut recovered
            } else {
                &mut degraded
            };
            for server in servers {
                target.insert(format!("{server}@{}", root.display()));
            }
        }
        for id in &degraded {
            writer.mark_degraded(id, "unavailable during diagnostics");
        }
        // A server that degraded any file stays degraded; clear only the ones
        // with no degraded file this run.
        for id in recovered.difference(&degraded) {
            writer.clear_degraded(id);
        }
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

/// Whether two paths denote the same location.
///
/// Compares canonically so a symlinked or non-canonical `.` still matches the
/// registered workspace-root value; falls back to a literal comparison when
/// either side won't canonicalize (a root that is gone from disk, say).
fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Converts a `file://` URI (as produced by [`crate::lsp::lang::path_to_uri`])
/// back to a filesystem path.
///
/// Catenary emits unencoded `file://<path>` URIs and the servers it drives echo
/// them, so a plain prefix strip is sufficient; a non-`file://` URI yields
/// `None`.
fn uri_to_pathbuf(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
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

/// The `source` field of a diagnostic, or `""` when absent.
fn source_of(diagnostic: &Value) -> &str {
    diagnostic
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Cross-source dedup keeping the highest-weight source's copy (linters ticket
/// 05).
///
/// Collapses findings that are the *same* — keyed coarse on `(code, start-line)`,
/// codeless fallback `(normalized-message, line)`. The key **drops `source`**, so
/// the same finding reported by two different sources collapses: bash-ls's
/// wrapped shellcheck `SC2086` and standalone shellcheck's `SC2086`, or a real
/// error reported by both rust-analyzer-native and rustc-flycheck. Anchored on
/// line, not column/span — LSP (0-based char) and CLI (1-based) ranges drift and
/// a wrapper may normalize spans differently; bias **coarse** (over-dedup on a
/// tie beats leaking duplicates, since the aggregator owns the clean output).
///
/// When a group spans multiple sources, the **highest-weight** source's copy is
/// kept; ties fall to first-seen (the entry order is feeder order — LSP feeders
/// before linters). Surviving entries keep their original relative order.
fn dedupe_entries(
    entries: Vec<FeederEntry>,
    weights: &crate::config::DiagnosticWeights,
) -> Vec<FeederEntry> {
    // Map each dedup key to the index of its current keeper (heaviest source so
    // far, first-seen on a tie).
    let mut keeper: std::collections::HashMap<(String, u32), usize> =
        std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let key = dedup_key(&e.value);
        match keeper.get(&key) {
            None => {
                keeper.insert(key, i);
            }
            Some(&j) => {
                // Strictly-greater so a tie keeps the earlier (first-seen) entry.
                if weights.weight(source_of(&e.value))
                    > weights.weight(source_of(&entries[j].value))
                {
                    keeper.insert(key, i);
                }
            }
        }
    }
    let keep: HashSet<usize> = keeper.into_values().collect();
    entries
        .into_iter()
        .enumerate()
        .filter_map(|(i, e)| keep.contains(&i).then_some(e))
        .collect()
}

/// Drops *provisional* findings that are challenged but uncorroborated (linters
/// ticket 05) — the misc-115 phantom and nothing else.
///
/// Runs over the **post-dedup** set. A finding is provisional when its
/// `(source, code)` falls in a source's provisional band
/// ([`DiagnosticWeights::is_provisional`](crate::config::DiagnosticWeights::is_provisional)).
/// A provisional finding is dropped only when **challenged** — a strictly-heavier
/// source reported *anything* for the file. It survives when:
///
/// - **corroborated** — a heavier source emitted the same finding, in which case
///   dedup already kept the heavier copy and dropped this one, so any provisional
///   entry still present here is uncorroborated by construction; or
/// - **unchallenged** — no strictly-heavier source reported for the file (the
///   instant pre-flycheck preview, a single-source server).
///
/// The "challenged" test reuses the weights: the file's max present weight is
/// strictly greater than the provisional source's weight. Non-provisional
/// findings (out-of-band native lints, every linter finding) are untouched.
fn drop_challenged_provisional(
    entries: Vec<FeederEntry>,
    weights: &crate::config::DiagnosticWeights,
) -> Vec<FeederEntry> {
    let Some(max_weight) = entries
        .iter()
        .map(|e| weights.weight(source_of(&e.value)))
        .max()
    else {
        return entries;
    };
    entries
        .into_iter()
        .filter(|e| {
            let source = source_of(&e.value);
            let code = render_diagnostic_code(e.value.get("code"));
            if !weights.is_provisional(source, &code) {
                return true;
            }
            // Provisional + uncorroborated (survived dedup): keep iff unchallenged
            // — no strictly-heavier source reported for the file.
            max_weight <= weights.weight(source)
        })
        .collect()
}

/// Builds the cross-source dedup key for a diagnostic: `(discriminant, line)`.
///
/// `line` is the 0-based start line. The discriminant is the rendered code when
/// present (`c\0<code>`), else the normalized message (`m\0<message>`) — the NUL
/// tag keeps a code that happens to equal a message text from colliding across
/// the two key shapes. The `source` is deliberately **not** part of the key, so a
/// finding reported by multiple sources collapses (ticket 05).
fn dedup_key(diagnostic: &Value) -> (String, u32) {
    let line = crate::lsp::extract::diagnostic_range(diagnostic).map_or(0, |r| r.start.line);
    let code = render_diagnostic_code(diagnostic.get("code"));
    let discriminant = if code.is_empty() {
        let message = crate::lsp::extract::diagnostic_message(diagnostic).unwrap_or("");
        format!("m\u{0}{}", normalize_message(message))
    } else {
        format!("c\u{0}{code}")
    };
    (discriminant, line)
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

/// Classifies a named path the LSP-awareness gate declined into its receipt
/// category (bug 58 / ephemeral-roots ticket 01).
///
/// The gate ([`PathValidator::validate_read`]) fails a path for one of two
/// reasons, and the receipt must say which: the path does not exist (it will
/// not canonicalize → [`OutOfScopeKind::Missing`]), or it exists but resolves
/// outside every mounted root ([`OutOfScopeKind::OutsideRoots`]). For the
/// latter, the enclosing project root is detected by walking repository markers
/// (`.git`/`.svn`/`.hg`/`.jj`) up from the path (the general anchor,
/// [`crate::companions::enclosing_worktree_root`]) so
/// the receipt can name what a `catenary pin` would mount; a path with no
/// enclosing project root carries `None`.
fn classify_out_of_scope(resolved: &std::path::Path) -> OutOfScopeEntry {
    resolved.canonicalize().map_or_else(
        |_| OutOfScopeEntry {
            display: super::compress_home(resolved),
            kind: OutOfScopeKind::Missing,
        },
        |canonical| {
            let enclosing_root = crate::companions::enclosing_worktree_root(&canonical);
            OutOfScopeEntry {
                display: super::compress_home(&canonical),
                kind: OutOfScopeKind::OutsideRoots { enclosing_root },
            }
        },
    )
}

/// Renders the out-of-scope named-path lines for a scoped receipt.
///
/// One line per named path that could not be scoped, never a silent drop (bug
/// 58 / ephemeral-roots ticket 01). The lines are flat — these paths have no
/// mounted root to group under — and each says *why* there is no verdict:
///
/// - nonexistent → `<path> [path does not exist]`;
/// - outside every root, enclosing project root detectable → `<path> [no
///   language servers running for <root> — not a mounted root; see `catenary
///   roots -h`]`;
/// - outside every root, no enclosing project root → `<path> [outside every
///   mounted root; see `catenary roots -h`]`.
///
/// Returns the empty string when there is nothing out of scope, so the caller
/// appends it unconditionally without a trailing blank line.
fn render_out_of_scope(entries: &[OutOfScopeEntry]) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for entry in entries {
        match &entry.kind {
            OutOfScopeKind::Missing => {
                _ = writeln!(out, "{} [path does not exist]", entry.display);
            }
            OutOfScopeKind::OutsideRoots {
                enclosing_root: Some(root),
            } => {
                _ = writeln!(
                    out,
                    "{} [no language servers running for {} \u{2014} not a mounted root; see `catenary roots -h`]",
                    entry.display,
                    super::compress_home(root),
                );
            }
            OutOfScopeKind::OutsideRoots {
                enclosing_root: None,
            } => {
                _ = writeln!(
                    out,
                    "{} [outside every mounted root; see `catenary roots -h`]",
                    entry.display,
                );
            }
        }
    }
    out
}

/// Formats the per-file receipt.
///
/// Bare root-path section headers. Every diagnosed file is listed: dirty files
/// with their diagnostics beneath, clean files with a `[clean]` line beside the
/// path, unverified files with an `[unverified — <server> returned no result]`
/// line, uncovered files noted with `[no LSP coverage]`. Clean is **explicit**,
/// never silence (workstream 37 ticket 01, retiring misc 111 / decision 022) —
/// the receipt is proof of the debt the run paid, so every file it diagnosed
/// appears and counts toward the collapse total. An `unverified` file (its
/// server died before producing a result) appears for the same reason (bug 56):
/// an all-`NoResults` set must never render as empty stdout.
///
/// When a root contains a single (printed) file, the root and filename
/// are collapsed into one path (e.g. `/tmp/scratch.sh`). Multi-file
/// roots get a directory header with indented file entries beneath.
/// Root headers are only emitted for roots that have something to print.
///
/// `collapse_clean` (a directory/whole-root scope, ticket 04) replaces a
/// multi-file root's per-file `[clean]` lines with a single `N files clean`
/// count, and its per-file unverified lines with an `M files unverified` count —
/// the dirty files are the signal. It never touches diagnostics or the
/// single-file collapsed path (a lone clean or unverified file still shows its
/// name).
///
/// When any file is unverified, the receipt **opens with an `unavailable:
/// <server>` banner** ([`prepend_unavailable_banner`], decision 027) naming the
/// server(s) that degraded, so degraded coverage never reads as clean.
#[allow(
    clippy::too_many_lines,
    reason = "one linear renderer: the four file categories (dirty / clean / unverified / uncovered) each render in the collapsed and multi-file branches, top-to-bottom"
)]
fn format_diagnostics(
    diag_files: &[DiagnosticFile],
    uncovered: &[UncoveredEntry],
    clean: &[CleanEntry],
    unverified: &[UnverifiedEntry],
    collapse_clean: bool,
) -> String {
    use std::fmt::Write;

    let mut root_diag: BTreeMap<&PathBuf, Vec<(&str, &[DiagEntry])>> = BTreeMap::new();
    let mut root_clean: BTreeMap<&PathBuf, Vec<&str>> = BTreeMap::new();
    let mut root_unverified: BTreeMap<&PathBuf, Vec<(&str, &str)>> = BTreeMap::new();
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
    for ue in unverified {
        root_unverified
            .entry(&ue.root)
            .or_default()
            .push((&ue.display, &ue.server));
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
    all_roots.extend(root_unverified.keys());
    all_roots.extend(root_uncovered.keys());

    let mut output = String::new();

    for root in &all_roots {
        let diag_count = root_diag.get(root).map_or(0, Vec::len);
        let clean_count = root_clean.get(root).map_or(0, Vec::len);
        let unverified_count = root_unverified.get(root).map_or(0, Vec::len);
        let uncovered_count = root_uncovered.get(root).map_or(0, Vec::len);
        let total = diag_count + clean_count + unverified_count + uncovered_count;
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
                    _ = writeln!(output, "{} [clean]", root.join(f).display());
                }
            }
            if let Some(unv_files) = root_unverified.get(root) {
                for (display, server) in unv_files {
                    _ = writeln!(
                        output,
                        "{} [unverified \u{2014} {server} returned no result]",
                        root.join(display).display()
                    );
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
                if collapse_clean {
                    // Directory/whole-root scope: fold the clean list to a count.
                    let n = clean_files.len();
                    let plural = if n == 1 { "" } else { "s" };
                    _ = writeln!(output, "\t{n} file{plural} clean");
                } else {
                    for f in clean_files {
                        _ = writeln!(output, "\t{f} [clean]");
                    }
                }
            }
            if let Some(unv_files) = root_unverified.get(root) {
                if collapse_clean {
                    // Directory/whole-root scope: fold the unverified list to a
                    // count alongside the clean count (bug 56).
                    let n = unv_files.len();
                    let plural = if n == 1 { "" } else { "s" };
                    _ = writeln!(output, "\t{n} file{plural} unverified");
                } else {
                    for (display, server) in unv_files {
                        _ = writeln!(
                            output,
                            "\t{display} [unverified \u{2014} {server} returned no result]"
                        );
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

    prepend_unavailable_banner(unverified, output)
}

/// Prepends the `unavailable: <server>` banner to a rendered receipt when the
/// run degraded a server's coverage (decision 027).
///
/// The unavailable servers are exactly those the unverified files name — a
/// [`FileOutcome::NoResults`] file means its assigned server produced nothing
/// (it died mid-run, or failed to start). A completed run that degraded a
/// server therefore opens with a top-line banner naming it, so degraded never
/// reads as clean; the per-file `[unverified — …]` lines (or the `M files
/// unverified` collapse) stay beneath. Restrained wording — name the server,
/// never dump internal state — and one line per distinct server, sorted. With
/// no unverified files there is no banner (a fully-recovered run is silent
/// about the transient death).
fn prepend_unavailable_banner(unverified: &[UnverifiedEntry], body: String) -> String {
    use std::fmt::Write;

    let mut servers: BTreeSet<&str> = BTreeSet::new();
    for ue in unverified {
        for name in ue.server.split(", ") {
            if !name.is_empty() {
                servers.insert(name);
            }
        }
    }
    if servers.is_empty() {
        return body;
    }

    let mut out = String::new();
    for name in &servers {
        _ = writeln!(out, "unavailable: {name}");
    }
    out.push_str(&body);
    out
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
        let output = format_diagnostics(&diag_files, &[], &[], &[], false);
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
        let output = format_diagnostics(&diag_files, &[], &[], &[], false);
        // All entries should be present (no paging).
        for i in 0..5 {
            assert!(output.contains(&format!("msg {i}")), "output: {output}");
        }
    }

    #[test]
    fn format_empty_batch_is_empty() {
        // A batch with no diagnosed files at all — no dirty, clean, or
        // uncovered entries — renders nothing. The empty-set sentinel
        // (`[no edited files]`) is the CLI's job, not the formatter's.
        let output = format_diagnostics(&[], &[], &[], &[], false);
        assert!(output.is_empty(), "expected empty output, got: {output:?}");
    }

    #[test]
    fn format_clean_files_listed_explicitly() {
        // Clean is explicit, never silence (ws37 ticket 01, retiring misc 111):
        // a verified-clean file carries a `[clean]` line beside its path.
        let clean = vec![CleanEntry {
            display: "file.rs".to_string(),
            root: PathBuf::from("/test"),
        }];
        let output = format_diagnostics(&[], &[], &clean, &[], false);
        // Single file under root → collapsed path with `[clean]` beside it.
        assert_eq!(output.trim(), "/test/file.rs [clean]", "output: {output}");
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
        let output = format_diagnostics(&diag_files, &[], &[], &[], false);
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
    fn format_clean_listed_beside_dirty_in_mixed_batch() {
        // A dirty file and a clean sibling under one root both appear: the
        // dirty file lists its diagnostics beneath, the clean file carries a
        // `[clean]` line beside it (ws37 ticket 01 — clean is explicit).
        let diag_files = vec![DiagnosticFile {
            display: "src/lib.rs".to_string(),
            root: PathBuf::from("/alpha"),
            entries: vec![de(1, ":1:1 [error] test: alpha error")],
        }];
        let clean = vec![CleanEntry {
            display: "src/main.rs".to_string(),
            root: PathBuf::from("/alpha"),
        }];
        let output = format_diagnostics(&diag_files, &[], &clean, &[], false);
        // Two printed files under /alpha → directory header, indented entries.
        assert!(output.contains("/alpha\n"), "output: {output}");
        assert!(output.contains("\tsrc/lib.rs:"), "output: {output}");
        assert!(output.contains("\t\t:1:1 [error]"), "output: {output}");
        assert!(output.contains("\tsrc/main.rs [clean]"), "output: {output}");
    }

    #[test]
    fn format_single_file_server() {
        let diag_files = vec![DiagnosticFile {
            display: "scratch.sh".to_string(),
            root: PathBuf::from("/tmp"),
            entries: vec![de(2, ":3:1 [warning] test: standalone warning")],
        }];
        let output = format_diagnostics(&diag_files, &[], &[], &[], false);
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
        let output = format_diagnostics(&diag_files, &[], &[], &[], false);
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
        let output = format_diagnostics(&[], &uncovered, &[], &[], false);
        // Single file → collapsed path with [no LSP coverage].
        assert!(output.contains("/project/data.csv\n"), "output: {output}");
        assert!(output.contains("\t[no LSP coverage]"), "output: {output}");
    }

    #[test]
    fn format_clean_and_uncovered_both_listed() {
        // A clean covered file plus an uncovered file: the clean file carries a
        // `[clean]` line (ws37 ticket 01), and the uncovered note is preserved
        // (the LSP-unavailable signal, tickets 69/80). Two printed files under
        // /project → directory header with indented entries.
        let clean = vec![CleanEntry {
            display: "lib.rs".to_string(),
            root: PathBuf::from("/project"),
        }];
        let uncovered = vec![UncoveredEntry {
            display: "data.csv".to_string(),
            root: PathBuf::from("/project"),
        }];
        let output = format_diagnostics(&[], &uncovered, &clean, &[], false);
        assert!(output.contains("/project\n"), "output: {output}");
        assert!(output.contains("\tlib.rs [clean]"), "output: {output}");
        assert!(output.contains("\tdata.csv"), "output: {output}");
        assert!(output.contains("\t\t[no LSP coverage]"), "output: {output}");
    }

    // ── clean-collapse rendering (ws37 ticket 04) ─────────────────

    #[test]
    fn format_collapses_clean_list_when_directory_scoped() {
        // A directory/whole-root scope folds the per-file `[clean]` list to a
        // single count — the dirty files are the signal (decision 5). The
        // individual clean filenames must NOT appear.
        let clean: Vec<CleanEntry> = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|f| CleanEntry {
                display: (*f).to_string(),
                root: PathBuf::from("/project"),
            })
            .collect();
        let output = format_diagnostics(&[], &[], &clean, &[], true);
        assert!(output.contains("/project\n"), "output: {output}");
        assert!(output.contains("\t3 files clean"), "output: {output}");
        assert!(!output.contains("a.rs"), "clean names leaked: {output}");
        assert!(
            !output.contains("[clean]"),
            "per-file marker leaked: {output}"
        );
    }

    #[test]
    fn format_collapse_keeps_dirty_files_inline() {
        // Clean collapses to a count; the dirty file still lists every
        // diagnostic beneath it (decision 025 — diagnostics are never collapsed).
        let diag_files = vec![DiagnosticFile {
            display: "broken.rs".to_string(),
            root: PathBuf::from("/project"),
            entries: vec![de(1, ":1:1 [error] test: boom")],
        }];
        let clean: Vec<CleanEntry> = (0..5)
            .map(|i| CleanEntry {
                display: format!("ok{i}.rs"),
                root: PathBuf::from("/project"),
            })
            .collect();
        let output = format_diagnostics(&diag_files, &[], &clean, &[], true);
        assert!(output.contains("\tbroken.rs:"), "output: {output}");
        assert!(
            output.contains("\t\t:1:1 [error] test: boom"),
            "output: {output}"
        );
        assert!(output.contains("\t5 files clean"), "output: {output}");
        assert!(!output.contains("ok0.rs"), "clean names leaked: {output}");
    }

    #[test]
    fn format_singular_clean_count_when_collapsed() {
        // A single clean file under a multi-file root still collapses (the
        // grammar is singular). Here one dirty + one clean → multi-file branch.
        let diag_files = vec![DiagnosticFile {
            display: "broken.rs".to_string(),
            root: PathBuf::from("/project"),
            entries: vec![de(1, ":1:1 [error] test: boom")],
        }];
        let clean = vec![CleanEntry {
            display: "ok.rs".to_string(),
            root: PathBuf::from("/project"),
        }];
        let output = format_diagnostics(&diag_files, &[], &clean, &[], true);
        assert!(output.contains("\t1 file clean"), "output: {output}");
        assert!(!output.contains("ok.rs"), "clean name leaked: {output}");
    }

    // ── unverified rendering (bug 56) ─────────────────────────────

    /// Build an [`UnverifiedEntry`] (test ergonomics).
    fn ue(display: &str, root: &str, server: &str) -> UnverifiedEntry {
        UnverifiedEntry {
            display: display.to_string(),
            root: PathBuf::from(root),
            server: server.to_string(),
        }
    }

    #[test]
    fn format_unverified_file_listed_explicitly() {
        // A file whose server produced no result earns an explicit unverified
        // line beside its path — never silence (bug 56). Single file under root →
        // collapsed path.
        let unverified = vec![ue("file.rs", "/test", "rust-analyzer")];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        // The receipt opens with the `unavailable:` banner (decision 027),
        // then the per-file `[unverified — …]` line beneath it.
        assert_eq!(
            output.trim(),
            "unavailable: rust-analyzer\n\
             /test/file.rs [unverified \u{2014} rust-analyzer returned no result]",
            "output: {output}"
        );
    }

    #[test]
    fn format_all_unverified_never_empty() {
        // An all-`NoResults` set renders one unverified line per file — never the
        // empty stdout that used to be indistinguishable from a hang (bug 56).
        let unverified = vec![
            ue("src/a.rs", "/project", "rust-analyzer"),
            ue("src/b.rs", "/project", "rust-analyzer"),
        ];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        assert!(!output.trim().is_empty(), "must never be empty: {output:?}");
        // Two files under /project → directory header, indented per-file lines.
        assert!(output.contains("/project\n"), "output: {output}");
        assert!(
            output.contains("\tsrc/a.rs [unverified \u{2014} rust-analyzer returned no result]"),
            "output: {output}"
        );
        assert!(
            output.contains("\tsrc/b.rs [unverified \u{2014} rust-analyzer returned no result]"),
            "output: {output}"
        );
    }

    #[test]
    fn format_unverified_beside_clean_and_dirty() {
        // A dirty file, a clean sibling, and an unverified sibling under one root
        // all appear: diagnostics beneath the dirty one, `[clean]` beside the
        // clean one, and the unverified line beside the third (bug 56).
        let diag_files = vec![DiagnosticFile {
            display: "src/lib.rs".to_string(),
            root: PathBuf::from("/alpha"),
            entries: vec![de(1, ":1:1 [error] test: alpha error")],
        }];
        let clean = vec![CleanEntry {
            display: "src/main.rs".to_string(),
            root: PathBuf::from("/alpha"),
        }];
        let unverified = vec![ue("src/dead.rs", "/alpha", "rust-analyzer")];
        let output = format_diagnostics(&diag_files, &[], &clean, &unverified, false);
        // Three printed files under /alpha → directory header, indented entries.
        assert!(output.contains("/alpha\n"), "output: {output}");
        assert!(output.contains("\tsrc/lib.rs:"), "output: {output}");
        assert!(output.contains("\t\t:1:1 [error]"), "output: {output}");
        assert!(output.contains("\tsrc/main.rs [clean]"), "output: {output}");
        assert!(
            output.contains("\tsrc/dead.rs [unverified \u{2014} rust-analyzer returned no result]"),
            "output: {output}"
        );
    }

    #[test]
    fn format_collapses_unverified_when_directory_scoped() {
        // A directory/whole-root scope folds the per-file unverified list to a
        // single count line, alongside the clean count (bug 56). The individual
        // unverified filenames and per-file marker must NOT appear.
        let clean: Vec<CleanEntry> = ["a.rs", "b.rs"]
            .iter()
            .map(|f| CleanEntry {
                display: (*f).to_string(),
                root: PathBuf::from("/project"),
            })
            .collect();
        let unverified = vec![
            ue("x.rs", "/project", "rust-analyzer"),
            ue("y.rs", "/project", "rust-analyzer"),
            ue("z.rs", "/project", "rust-analyzer"),
        ];
        let output = format_diagnostics(&[], &[], &clean, &unverified, true);
        assert!(output.contains("/project\n"), "output: {output}");
        assert!(output.contains("\t2 files clean"), "output: {output}");
        assert!(output.contains("\t3 files unverified"), "output: {output}");
        assert!(
            !output.contains("x.rs"),
            "unverified names leaked: {output}"
        );
        assert!(
            !output.contains("[unverified"),
            "per-file marker leaked: {output}"
        );
    }

    #[test]
    fn format_singular_unverified_count_when_collapsed() {
        // A single unverified file under a multi-file collapsed root uses the
        // singular grammar. One dirty + one unverified → multi-file branch.
        let diag_files = vec![DiagnosticFile {
            display: "broken.rs".to_string(),
            root: PathBuf::from("/project"),
            entries: vec![de(1, ":1:1 [error] test: boom")],
        }];
        let unverified = vec![ue("dead.rs", "/project", "rust-analyzer")];
        let output = format_diagnostics(&diag_files, &[], &[], &unverified, true);
        assert!(output.contains("\t1 file unverified"), "output: {output}");
        assert!(
            !output.contains("dead.rs"),
            "unverified name leaked: {output}"
        );
    }

    // ── unavailable-server banner (decision 027) ──────────────────

    #[test]
    fn format_unavailable_banner_opens_receipt() {
        // A run with a degraded server opens with a top-line banner naming it,
        // and the per-file `[unverified — …]` line stays beneath (decision 027).
        let unverified = vec![ue("file.rs", "/test", "rust-analyzer")];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        let banner_pos = output
            .find("unavailable: rust-analyzer")
            .expect("banner present");
        let unverified_pos = output.find("[unverified").expect("unverified line present");
        assert!(
            banner_pos < unverified_pos,
            "banner must open the receipt, before the unverified lines: {output}"
        );
        // The banner is the very first line of the receipt.
        assert!(
            output.starts_with("unavailable: rust-analyzer\n"),
            "receipt opens with the banner: {output}"
        );
    }

    #[test]
    fn format_no_banner_without_unverified() {
        // A clean/dirty-only receipt (no unverified files, no degraded server)
        // carries no banner — a fully-recovered run is silent about it.
        let diag_files = vec![DiagnosticFile {
            display: "file.rs".to_string(),
            root: PathBuf::from("/test"),
            entries: vec![de(1, ":1:1 [error] test: boom")],
        }];
        let clean = vec![CleanEntry {
            display: "ok.rs".to_string(),
            root: PathBuf::from("/test"),
        }];
        let output = format_diagnostics(&diag_files, &[], &clean, &[], false);
        assert!(
            !output.contains("unavailable:"),
            "no banner without unverified files: {output}"
        );
    }

    #[test]
    fn format_banner_dedups_one_line_per_server() {
        // Several unverified files behind the same server collapse to a single
        // banner line — restrained, no per-file repetition.
        let unverified = vec![
            ue("src/a.rs", "/project", "rust-analyzer"),
            ue("src/b.rs", "/project", "rust-analyzer"),
            ue("src/c.rs", "/project", "rust-analyzer"),
        ];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        assert_eq!(
            output.matches("unavailable: rust-analyzer").count(),
            1,
            "one banner line per distinct server: {output}"
        );
    }

    #[test]
    fn format_banner_lists_each_distinct_server_sorted() {
        // Distinct unavailable servers each get a banner line, sorted.
        let unverified = vec![
            ue("a.jl", "/project", "julia-lsp"),
            ue("b.rs", "/project", "rust-analyzer"),
        ];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        let julia = output.find("unavailable: julia-lsp").expect("julia banner");
        let rust = output
            .find("unavailable: rust-analyzer")
            .expect("rust banner");
        assert!(julia < rust, "banner lines sorted: {output}");
    }

    #[test]
    fn format_banner_survives_directory_collapse() {
        // Even when the unverified list collapses to a count, the banner still
        // names the degraded server (derived from the entries, not the body).
        let unverified = vec![
            ue("x.rs", "/project", "rust-analyzer"),
            ue("y.rs", "/project", "rust-analyzer"),
        ];
        let output = format_diagnostics(&[], &[], &[], &unverified, true);
        assert!(
            output.starts_with("unavailable: rust-analyzer\n"),
            "banner opens the collapsed receipt: {output}"
        );
        assert!(
            output.contains("\t2 files unverified"),
            "collapsed count still beneath: {output}"
        );
    }

    #[test]
    fn prepend_unavailable_banner_splits_joined_servers() {
        // A file whose server field joins multiple dead servers yields one
        // banner line per server (split on ", "), deduped and sorted.
        let unverified = vec![ue("f.rs", "/p", "server-b, server-a")];
        let out = prepend_unavailable_banner(&unverified, String::from("BODY"));
        assert_eq!(
            out, "unavailable: server-a\nunavailable: server-b\nBODY",
            "each joined server banners once, sorted, above the body: {out}"
        );
    }

    // ── out-of-scope named paths (bug 58 / ephemeral-roots ticket 01) ──

    #[test]
    fn render_out_of_scope_empty_is_empty() {
        // Nothing out of scope → the appended string is empty, so the caller
        // never adds a stray blank line to an otherwise complete receipt.
        assert_eq!(render_out_of_scope(&[]), "");
    }

    #[test]
    fn render_out_of_scope_missing_line() {
        // A named path that does not exist renders the literal
        // `path does not exist`, closing the ws37-02 empty-stdout edge.
        let entries = vec![OutOfScopeEntry {
            display: "~/gone/file.rs".to_string(),
            kind: OutOfScopeKind::Missing,
        }];
        assert_eq!(
            render_out_of_scope(&entries),
            "~/gone/file.rs [path does not exist]\n",
        );
    }

    #[test]
    fn render_out_of_scope_names_enclosing_root() {
        // Outside every mounted root with a detectable enclosing project root:
        // the line names the root and says it is not mounted, pointing at the
        // command that would mount it.
        let entries = vec![OutOfScopeEntry {
            display: "~/Projects/Lattice/README.md".to_string(),
            kind: OutOfScopeKind::OutsideRoots {
                enclosing_root: Some(PathBuf::from("/home/dev/Projects/Lattice")),
            },
        }];
        let out = render_out_of_scope(&entries);
        assert!(
            out.starts_with("~/Projects/Lattice/README.md [no language servers running for "),
            "names the path and reason: {out}"
        );
        assert!(
            out.contains("Projects/Lattice \u{2014} not a mounted root"),
            "names the enclosing root and its unmounted status: {out}"
        );
        assert!(
            out.contains("see `catenary roots -h`]"),
            "points at the command that mounts it: {out}"
        );
        assert!(out.ends_with('\n'), "one complete line: {out:?}");
    }

    #[test]
    fn render_out_of_scope_plain_line_without_root() {
        // Outside every root with no detectable project root: a plain
        // out-of-scope line, same family, no root named.
        let entries = vec![OutOfScopeEntry {
            display: "/etc/hostname".to_string(),
            kind: OutOfScopeKind::OutsideRoots {
                enclosing_root: None,
            },
        }];
        assert_eq!(
            render_out_of_scope(&entries),
            "/etc/hostname [outside every mounted root; see `catenary roots -h`]\n",
        );
    }

    #[test]
    fn render_out_of_scope_one_line_per_path() {
        // A mixed set renders exactly one line per named path — never a silent
        // branch (the scoped-receipt contract).
        let entries = vec![
            OutOfScopeEntry {
                display: "~/a/missing.rs".to_string(),
                kind: OutOfScopeKind::Missing,
            },
            OutOfScopeEntry {
                display: "~/b/outside.rs".to_string(),
                kind: OutOfScopeKind::OutsideRoots {
                    enclosing_root: None,
                },
            },
        ];
        let out = render_out_of_scope(&entries);
        assert_eq!(out.lines().count(), 2, "one line per named path: {out}");
    }

    #[test]
    fn classify_out_of_scope_missing_path() {
        // A path that cannot be canonicalized is nonexistent, not out-of-root.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does_not_exist.rs");
        let entry = classify_out_of_scope(&missing);
        assert!(
            matches!(entry.kind, OutOfScopeKind::Missing),
            "nonexistent path classifies as Missing"
        );
    }

    #[test]
    fn classify_out_of_scope_detects_enclosing_git_root() {
        // An existing path outside every root whose ancestor carries `.git`
        // resolves to that enclosing project root (the `.git` general anchor).
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical tempdir");
        std::fs::create_dir(root.join(".git")).expect("create .git");
        let file = root.join("README.md");
        std::fs::write(&file, "# readme").expect("write file");

        let detected = match classify_out_of_scope(&file).kind {
            OutOfScopeKind::OutsideRoots { enclosing_root } => enclosing_root,
            OutOfScopeKind::Missing => None,
        };
        assert_eq!(
            detected.as_deref(),
            Some(root.as_path()),
            "detects the enclosing .git root"
        );
    }

    #[test]
    fn classify_out_of_scope_no_root_when_no_git() {
        // An existing path outside every root with no `.git` ancestor carries
        // no enclosing root → the plain out-of-scope line.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("loose.txt");
        std::fs::write(&file, "loose").expect("write file");
        let entry = classify_out_of_scope(&file);
        assert!(
            matches!(
                entry.kind,
                OutOfScopeKind::OutsideRoots {
                    enclosing_root: None
                }
            ),
            "no `.git` ancestor → no enclosing root"
        );
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

    // ── complete-report rendering (decision 025) ────────────────────

    #[test]
    fn format_diagnostics_renders_every_entry() {
        // No budget, no truncation: every diagnostic in the batch prints.
        let diag_files = vec![DiagnosticFile {
            display: "a.rs".to_string(),
            root: PathBuf::from("/r"),
            entries: vec![
                de(2, ":1:1 [warning] w: warn-a"),
                de(1, ":2:1 [error] e: err-b"),
                de(2, ":3:1 [warning] w: warn-c"),
            ],
        }];
        let out = format_diagnostics(&diag_files, &[], &[], &[], false);
        assert!(
            out.contains("warn-a") && out.contains("err-b") && out.contains("warn-c"),
            "the complete report keeps every diagnostic: {out}"
        );
    }

    // ── cross-feeder dedup + provisional tests (linters ticket 05) ──

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

    /// The shipped weight set: rust-analyzer native `10`, flycheck `100`,
    /// baseline `50`, provisional `^E[0-9]+$` on the native source.
    fn ra_weights() -> crate::config::DiagnosticWeights {
        crate::config::DiagnosticWeights::rust_analyzer_default()
    }

    #[test]
    fn dedup_collapses_same_finding_across_feeders() {
        // bash-language-server (an LSP feeder) and standalone shellcheck both
        // report SC2086 at the same line: same (code, line) → one entry.
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
        let deduped = dedupe_entries(entries, &ra_weights());
        assert_eq!(deduped.len(), 1, "the wrapped + standalone copy collapse");
        // Equal weight (both baseline) → first-seen (the LSP feeder) wins.
        assert_eq!(deduped[0].ctx.command, "bash-language-server");
    }

    #[test]
    fn dedup_anchors_on_line_not_column() {
        // Same code/line, drifting columns (LSP 0-based vs CLI 1-based) collapse
        // — the key is line-anchored, bias coarse.
        let entries = vec![
            fe("a", diag_at("sc", Some("SC1000"), 7, 0, "msg")),
            fe("b", diag_at("sc", Some("SC1000"), 7, 40, "msg")),
        ];
        assert_eq!(dedupe_entries(entries, &ra_weights()).len(), 1);
    }

    #[test]
    fn dedup_keeps_distinct_line_and_code() {
        // Different line and different code each stay (the key is `(code, line)`).
        let entries = vec![
            fe("a", diag_at("sc", Some("SC1000"), 7, 0, "msg")),
            fe("a", diag_at("sc", Some("SC1000"), 8, 0, "msg")), // different line
            fe("a", diag_at("sc", Some("SC1001"), 7, 0, "msg")), // different code
        ];
        assert_eq!(
            dedupe_entries(entries, &ra_weights()).len(),
            3,
            "nothing collapses"
        );
    }

    #[test]
    fn dedup_collapses_across_sources_keeping_heaviest() {
        // The same code at the same line from two *different* sources collapses
        // (source dropped from the key, ticket 05); the heavier-weight source's
        // copy is kept. A real error reported by both rust-analyzer (10) and
        // rustc (100) → one entry, the rustc copy.
        let entries = vec![
            fe(
                "rust-analyzer",
                diag_at("rust-analyzer", Some("E0599"), 3, 0, "no method foo"),
            ),
            fe(
                "rust-analyzer",
                diag_at("rustc", Some("E0599"), 3, 8, "no method named `foo`"),
            ),
        ];
        let kept = dedupe_entries(entries, &ra_weights());
        assert_eq!(kept.len(), 1, "cross-source duplicate collapses: {kept:?}");
        assert_eq!(entry_source(&kept[0]), "rustc", "heaviest source kept");
    }

    #[test]
    fn dedup_codeless_fallback_keys_on_normalized_message() {
        // No code → fall back to (normalized-message, line). Whitespace and case
        // differences in the message still collapse; a genuinely different
        // message does not.
        let entries = vec![
            fe("a", diag_at("yaml", None, 3, 0, "trailing   spaces")),
            fe("b", diag_at("yaml", None, 3, 4, "Trailing spaces")), // normalizes equal
            fe("c", diag_at("yaml", None, 3, 0, "wrong indentation")), // distinct
        ];
        let deduped = dedupe_entries(entries, &ra_weights());
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
        assert_eq!(dedupe_entries(entries, &ra_weights()).len(), 2);
    }

    #[test]
    fn provisional_phantom_dropped_when_challenged() {
        // Native E0107 phantom rides alongside a different rustc error. After
        // dedup (different codes, no collapse), the provisional native E0107 is
        // challenged (rustc, weight 100 > 10, reported) and uncorroborated → dropped.
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
        let kept = drop_challenged_provisional(entries, &ra_weights());
        assert_eq!(kept.len(), 1, "challenged phantom dropped: {kept:?}");
        assert_eq!(entry_source(&kept[0]), "rustc");
    }

    #[test]
    fn provisional_kept_when_unchallenged() {
        // Only the native preview reported (no heavier source). The provisional
        // E0107 is unchallenged → kept (the instant pre-flycheck preview).
        let entries = vec![fe(
            "rust-analyzer",
            diag_at("rust-analyzer", Some("E0107"), 0, 0, "preview"),
        )];
        let kept = drop_challenged_provisional(entries, &ra_weights());
        assert_eq!(kept.len(), 1, "unchallenged preview kept");
        assert_eq!(entry_source(&kept[0]), "rust-analyzer");
    }

    #[test]
    fn provisional_out_of_band_native_kept() {
        // A native lint outside the E#### band is not provisional, so it survives
        // even though heavier flycheck reported.
        let entries = vec![
            fe(
                "rust-analyzer",
                diag_at("rust-analyzer", Some("unused-variable"), 0, 0, "unused"),
            ),
            fe(
                "rust-analyzer",
                diag_at("rustc", Some("E0599"), 1, 0, "no method foo"),
            ),
        ];
        let kept = drop_challenged_provisional(entries, &ra_weights());
        assert_eq!(kept.len(), 2, "out-of-band native kept: {kept:?}");
    }

    #[test]
    fn provisional_corroborated_real_error_survives_as_heavier() {
        // A real E0599 reported by both native and rustc: dedup keeps the rustc
        // copy (heavier), and the provisional pass leaves rustc (non-provisional)
        // alone. The finding survives, labeled rustc.
        let entries = vec![
            fe(
                "rust-analyzer",
                diag_at("rust-analyzer", Some("E0599"), 3, 0, "no method foo"),
            ),
            fe(
                "rust-analyzer",
                diag_at("rustc", Some("E0599"), 3, 0, "no method named `foo`"),
            ),
        ];
        let weights = ra_weights();
        let kept = drop_challenged_provisional(dedupe_entries(entries, &weights), &weights);
        assert_eq!(kept.len(), 1, "corroborated real error survives: {kept:?}");
        assert_eq!(entry_source(&kept[0]), "rustc");
    }

    #[test]
    fn provisional_not_triggered_by_equal_weight_peer() {
        // A linter at baseline weight (50) does not challenge a baseline-weight
        // provisional-band-less source. With no provisional source present, the
        // pass is a no-op. Guards against the challenge firing on equal weights.
        let entries = vec![
            fe(
                "shellcheck",
                diag_at("shellcheck", Some("SC2086"), 0, 0, "quote"),
            ),
            fe(
                "yamllint",
                diag_at("yamllint", None, 1, 0, "trailing spaces"),
            ),
        ];
        let kept = drop_challenged_provisional(entries, &ra_weights());
        assert_eq!(kept.len(), 2, "non-provisional findings untouched");
    }
}
