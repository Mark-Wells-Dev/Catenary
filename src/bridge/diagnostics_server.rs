// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, idle detection, the retrieval evidence bar
//! ([`await_publish_evidence`] — a never-heard file on a demonstrated-push
//! server may not render `[clean]` until its publish arrives), diagnostics
//! retrieval (push cache first, pull fallback), severity filtering, noise
//! filtering, quick-fix collection, and compact formatting.

use super::filesystem_manager::{FilesystemManager, observe_mtime};
use super::linter::DiagnosticFeeder;
use super::path_security::PathValidator;
use crate::lsp::server::LspServer;
use crate::lsp::settle::{IdleDetector, POLL_INTERVAL, SettleResult, await_idle, tree_working};
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

/// LSP `MethodNotFound` (JSON-RPC standard) — the only pull-error evidence that
/// justifies a **permanent** capability downgrade: the method is genuinely
/// unsupported, so `textDocument/diagnostic` will never work on this connection
/// (bug 84). Every other pull error is transient and downgrades nothing.
const LSP_METHOD_NOT_FOUND: i64 = -32601;

/// LSP `ServerCancelled` (3.17) — "I could not serve this request right now."
/// The spec pairs it with `DiagnosticServerCancellationData` carrying a
/// `retriggerRequest` flag: when set (or absent — the safe default), the client
/// should re-issue the same request. The pull loop honours this, bounded, so a
/// busy server is retried without an unbounded spin (bug 84).
const LSP_SERVER_CANCELLED: i64 = -32802;

/// Bound on `ServerCancelled` re-triggers within a single file's pull.
///
/// Honest, finite: a server that stays busy past this many re-issues leaves the
/// debt **unsettled** (never a silent zero) rather than looping forever. No wall
/// clock gates the loop — each re-trigger is a fresh request the server answers
/// (with data, a fresh cancel, or a different error), and the transport layer's
/// own death/stuck detection bounds each individual request.
const PULL_RETRIGGER_LIMIT: u32 = 3;

/// The outcome of a pull attempt against a pull-discipline server.
///
/// A pull **settles** a debt directly (the design frame): a returned report —
/// even an empty one — is the server's on-demand verdict for the current text.
/// An **error** leaves the debt unsettled (never `Clean`, never an empty-vec
/// placeholder — bug 84's retired mechanism); the caller skips recording the
/// file so it resolves [`FileOutcome::NoResults`] and renders the honest
/// `[unverified — <server> returned no result]` line.
#[derive(Debug)]
enum PullSettlement {
    /// The pull returned a report — the debt settles with these diagnostics
    /// (possibly empty: an evidence-backed clean).
    Settled(Vec<Value>),
    /// The pull errored (rejected, cancelled past the retrigger bound, or a
    /// transport fault) — the debt stays unsettled.
    Unsettled,
}

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
    /// The canonical paths of every file this round actually diagnosed — the
    /// fan-out's covered set plus the workspace pull's reported documents.
    /// A directory argument expands to files *inside* the pipeline
    /// ([`DiagnosticsServer::plan_scope`]), so the caller's named-path set does
    /// not know them; the delivery seam unlinks this set from the debt ledger
    /// alongside the named paths so a file served through a directory argument
    /// is paid like one named directly (bug 120).
    pub served: Vec<PathBuf>,
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
    /// enclosing project root is found. Rarely reached for a scoped serve as
    /// of misc 203: an explicitly named target auto-mounts its enclosing root
    /// (markerless dirs included) before the pipeline runs, so this arm
    /// remains for the mounts that could not happen — a sensitive-path
    /// refusal (ws43-05), or a daemon with no root tracker.
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
    /// Why the file went unverified — drives the receipt wording (misc 160
    /// leg 1 / bug 79; strike states misc 167). The gate escape is the
    /// render's honesty, not a semantic change — every cause returns the
    /// receipt and pays the debt ("paying is diagnosing").
    cause: UnverifiedCause,
}

/// The typed cause behind an unverified receipt line, softest first.
///
/// The ordering is load-bearing: a file owed by several servers renders the
/// **softest** cause among them (`min`) — "stuck" or "benched" are claims
/// about a process, made only when every owing server earns them (a single
/// still-alive owner that merely returned nothing keeps `Silent`'s wording,
/// misc 160 leg 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum UnverifiedCause {
    /// The owing server was alive and simply produced nothing:
    /// `[unverified — <server> returned no result]`.
    Silent,
    /// The owing server's **verified discipline required a response this round**
    /// and none arrived — a declared-push server that never published, or a
    /// debounce-discipline server whose version echo never landed inside the
    /// declared bound (diagnostics-debt 05 / DESIGN §"The floor is fault
    /// attribution"). A blessed-set privilege: the discipline is the evidence, so
    /// the fault is attributed to the server and the round strikes the ledger
    /// (`[<server> did not answer for this round — its verified behavior requires
    /// a response; treating as a server fault, re-run to retry]`). Softer than the
    /// process-state faults below (their terminal lifecycle is harder evidence)
    /// but harder than [`Self::Silent`]: it survives the softest-`min` only when
    /// every owing server at least owed an answer, so a merely-silent co-owner
    /// still keeps `Silent`'s wording.
    ContractViolation,
    /// Process-state evidence types the owing server as stuck (respawn-dead /
    /// init-hung) with strikes remaining, so the next demand retries
    /// (misc 167): `[unverified — <server> stuck; will retry on demand]`.
    Stuck,
    /// Struck out with zero served requests ever (misc 167):
    /// `[broken — <server> never started]`.
    BenchedBroken,
    /// Struck out after previously serving (misc 167):
    /// `[unstable — <server> gave up after repeated crashes]`.
    BenchedUnstable,
}

impl UnverifiedCause {
    /// Maps the manager's strike-ledger verdict onto the receipt cause for a
    /// server whose unavailability is already established (dead tombstone or
    /// respawn-dead) — never `Silent`.
    const fn from_verdict(verdict: crate::lsp::manager::ReviveVerdict) -> Self {
        use crate::lsp::manager::ReviveVerdict;
        match verdict {
            ReviveVerdict::Revivable => Self::Stuck,
            ReviveVerdict::BenchedNeverStarted => Self::BenchedBroken,
            ReviveVerdict::BenchedUnstable => Self::BenchedUnstable,
        }
    }
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

/// The fault a server's batch (with recovery) exhibited this round, driving the
/// receipt's `[unverified — …]` cause and the strike ledger
/// (diagnostics-debt 05).
///
/// Returned by [`DiagnosticsServer::run_server_batch_with_recovery`]. A healthy
/// run — or one whose bounded revive recovered the whole remainder — yields
/// `None`; the two fault arms are mutually exclusive per server per round,
/// process-state death taking precedence over a contract violation (a dead
/// server can't be judged for not answering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchFault {
    /// The server died (terminal lifecycle) with an unretrieved remainder and
    /// the one bounded, strike-gated revive could not bring it back this run —
    /// process-state evidence it is stuck (misc 160 leg 1 / misc 167). `fan_out`
    /// types it with the server's [`crate::lsp::manager::ReviveVerdict`].
    RespawnDead,
    /// The server stayed **alive** but its **verified discipline owed an answer
    /// this round and none came** ([`LspServer::owes_answer`]): a declared-push
    /// server that never published, or a debounce-discipline server whose version
    /// echo never landed inside the declared bound (diagnostics-debt 05 / DESIGN
    /// §"The floor is fault attribution"). `fan_out` renders the
    /// [`UnverifiedCause::ContractViolation`] wording and strikes the same ledger
    /// a crashing server feeds — a server violating its adapter is sick the same
    /// way a crashing one is.
    ContractViolation,
}

/// One batch file's state within a diagnostics round on one server
/// (diagnostics-debt 01).
///
/// `synced` records whether this round sent sync traffic (`didOpen` /
/// `didChange`) for the file — the change gate's verdict. An unsynced file
/// is held open with content the server already has; whether it still owes
/// a `didSave` is a registry question
/// ([`LspClient::document_needs_save`]), asked at save time.
struct RoundDoc {
    /// Canonical filesystem path.
    path: PathBuf,
    /// Document URI on the server.
    uri: String,
    /// Whether this round sent `didOpen`/`didChange` for the file.
    synced: bool,
}

/// What a diagnostics round delivered to one server — the retrieval evidence
/// bar's arming input plus the diff floor arm's trigger signal (diagnostics-debt
/// 01 / misc 196).
///
/// Returned by [`DiagnosticsServer::settle_and_save`] on a round that reached
/// retrieval. `stimulated` (the pre-existing signal) is `true` when the round
/// sent any traffic (sync or save). `saved` is `true` when a `didSave` was
/// delivered to the server this round — the diff-discipline server's contractual
/// trigger: an alive diff server silent after a delivered save violates its
/// contract, while a diff round that delivered no save owes nothing (the floor's
/// diff arm, DESIGN §"Publisher-discipline metadata").
#[derive(Debug, Clone, Copy, Default)]
struct RoundStimulus {
    /// The round sent any traffic (sync or save) — arms the retrieval evidence
    /// bar (unchanged from the prior `stimulated` bool).
    stimulated: bool,
    /// A `didSave` was delivered to the server this round — the diff floor arm's
    /// trigger.
    saved: bool,
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
        owner: Option<&str>,
    ) -> DiagnosticsOutcome {
        if files.is_empty() {
            return DiagnosticsOutcome {
                output: String::new(),
                dirty: false,
                errors: 0,
                warnings: 0,
                served: Vec::new(),
            };
        }

        // ── Phase 0: scope routing ─────────────────────────────────
        let plan = self.plan_scope(files).await;

        let mut feeds: BTreeMap<String, FileFeed> = BTreeMap::new();

        // Fan-out engine (ticket 02's path): explicit files plus sub-root /
        // no-capability directories expanded to their covered files. Also yields
        // the per-file server assignment, so a file that dies before producing a
        // result can name the server(s) that owed it one (bug 56).
        let (mut canonical_paths, uncovered, out_of_scope, mut path_servers, mut stuck_servers) =
            self.fan_out(&plan.fan_out, parent_id, owner, &mut feeds)
                .await;

        // Whole-root + capable scopes: one workspace/diagnostic request each,
        // merged into the same per-file map the fan-out populated. A scan server
        // that refuses/fails its whole-workspace pull while alive feeds a
        // verified-contract violation into the same `stuck_servers` / `path_servers`
        // maps the fan-out populates — the floor's scan arm (misc 196).
        for (root, clients) in &plan.workspace_targets {
            self.workspace_pull(
                root,
                clients,
                &mut feeds,
                &mut canonical_paths,
                &mut path_servers,
                &mut stuck_servers,
            )
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
            &stuck_servers,
            plan.directory_scoped,
        );

        // ── Phase 4: invalidate caches ────────────────────────────
        self.fs.bump_generations(&canonical_paths);

        outcome
    }

    /// The per-file fan-out lifecycle: the always-available diagnostics engine
    /// (ticket 02's path), now factored behind the ticket-04 scope router.
    ///
    /// Pipeline: resolve + canonicalize → group by server → per server (the
    /// held-open batch round: change-gated didOpen/didChange → settle → health
    /// probe → didSave the unsaved → settle → retrieve per file; documents stay
    /// open — diagnostics-debt 01) → linter feeders. Populates `feeds` with each file's raw feeder
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
        owner: Option<&str>,
        feeds: &mut BTreeMap<String, FileFeed>,
    ) -> (
        Vec<PathBuf>,
        Vec<UncoveredEntry>,
        Vec<OutOfScopeEntry>,
        BTreeMap<String, BTreeSet<String>>,
        BTreeMap<String, UnverifiedCause>,
    ) {
        if files.is_empty() {
            return (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                BTreeMap::new(),
                BTreeMap::new(),
            );
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

        // Diagnostic servers whose lifecycle is terminal — process-state evidence
        // they are **stuck** (respawn-dead or init-hung), not merely silent,
        // each typed with its strike-ledger cause (misc 167): still revivable
        // renders `[unverified — <server> stuck; will retry on demand]`,
        // struck out renders the terminal `[broken — …]` / `[unstable — …]`
        // labels. Either way the escape pays the gate by returning the honest
        // receipt (misc 160 leg 1 / bug 79). Seeded here with the pre-batch
        // dead tombstones; the mid-run recovery adds respawn-deaths.
        let mut stuck_servers: BTreeMap<String, UnverifiedCause> = BTreeMap::new();

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
                    for (name, verdict) in dead_servers {
                        // A dead tombstone before the batch even ran is a
                        // terminal-lifecycle server (`unavailable_diagnostic_servers`
                        // returns only `is_terminal()`/not-alive instances or
                        // strike-recorded spawn failures): typed with its
                        // strike-ledger verdict (misc 167) — stuck-but-
                        // revivable vs benched.
                        stuck_servers.insert(name.clone(), UnverifiedCause::from_verdict(verdict));
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
        for (name, (client_mutex, paths)) in &server_groups {
            match self
                .run_server_batch_with_recovery(client_mutex, paths, parent_id, owner, feeds)
                .await
            {
                // The server ended terminal with an unretrieved remainder and
                // the one bounded respawn could not revive it this run
                // (respawn-dead) — typed with its strike-ledger verdict
                // (misc 167): stuck-but-retried-next-demand, or benched.
                Some(BatchFault::RespawnDead) => {
                    let verdict = {
                        let key = client_mutex.lock().await.server().key();
                        key.map_or(crate::lsp::manager::ReviveVerdict::Revivable, |k| {
                            self.client_manager.revive_verdict(&k)
                        })
                    };
                    stuck_servers.insert(name.clone(), UnverifiedCause::from_verdict(verdict));
                }
                // The server stayed alive but its verified discipline owed an
                // answer this round and gave none (declared-push or debounce) —
                // a verified-contract violation (diagnostics-debt 05). The
                // strike already landed in `run_server_batch_with_recovery`;
                // render the fault-attribution wording.
                Some(BatchFault::ContractViolation) => {
                    stuck_servers.insert(name.clone(), UnverifiedCause::ContractViolation);
                }
                None => {}
            }
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

        (
            canonical_paths,
            uncovered,
            out_of_scope,
            path_servers,
            stuck_servers,
        )
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
    ///
    /// **The scan floor arm (misc 196).** A [`scan`-discipline](crate::recipes::Discipline::Scan)
    /// server (marksman-class) owes its **whole-workspace answer**: this pull is
    /// its contractual trigger. When `workspace/diagnostic` goes unanswered or
    /// refused while the server is **alive**, that is a verified-contract violation
    /// (DESIGN's scan row: "silence ≠ clean; fault attribution below"). The arm
    /// records the strike (the same ledger a crash feeds) and surfaces a
    /// root-scoped `[unverified — … did not answer]` line via `stuck_servers` /
    /// `path_servers`, so a scan server's refused pull can never collapse to
    /// `[clean]` over its genuinely-unscanned root — it names the server, the
    /// cause, and the re-run action. A NON-scan server that merely fails the
    /// workspace pull (or a dead server — its unavailability is the fan-out's
    /// respawn-dead territory, not this seam) is left as the prior skip: not every
    /// server owes a workspace answer.
    async fn workspace_pull(
        &self,
        root: &std::path::Path,
        clients: &[Arc<Mutex<LspClient>>],
        feeds: &mut BTreeMap<String, FileFeed>,
        canonical_paths: &mut Vec<PathBuf>,
        path_servers: &mut BTreeMap<String, BTreeSet<String>>,
        stuck_servers: &mut BTreeMap<String, UnverifiedCause>,
    ) {
        for client_mutex in clients {
            let client = client_mutex.lock().await;
            let server_name = client.server_name().to_string();
            let ctx = Arc::new(FeederContext {
                command: client.server_command().to_string(),
                version: client.server_version().map(str::to_string),
                language_id: client.language().to_string(),
            });
            let served_key = client.server().key();
            let reports = match client.workspace_diagnostics().await {
                Ok(reports) => reports,
                Err(e) => {
                    // The scan floor arm (misc 196): an ALIVE scan server that
                    // refuses/fails its owed whole-workspace answer is a
                    // verified-contract violation — never a silent skip that lets
                    // the root read `[clean]`. A dead server, or a non-scan server
                    // that merely fails the pull, keeps the prior skip.
                    let scan_violation = client.server().is_scan() && client.is_alive();
                    drop(client);
                    if scan_violation {
                        warn!(
                            server = %server_name,
                            "scan server refused its owed workspace/diagnostic pull: {e} \
                             — attributing as a verified-contract violation",
                        );
                        if let Some(key) = &served_key {
                            self.client_manager.record_contract_violation(key);
                        }
                        // Surface the root itself as an unverified line naming the
                        // scan server: the root is in `canonical_paths` + owed by
                        // the server (`path_servers`) but absent from `feeds`, so it
                        // classifies `NoResults` and renders the ContractViolation
                        // wording.
                        let key = root.to_string_lossy().to_string();
                        path_servers
                            .entry(key)
                            .or_default()
                            .insert(server_name.clone());
                        stuck_servers.insert(server_name, UnverifiedCause::ContractViolation);
                        if !canonical_paths.contains(&root.to_path_buf()) {
                            canonical_paths.push(root.to_path_buf());
                        }
                    } else {
                        debug!(
                            server = %server_name,
                            "workspace/diagnostic failed, skipping: {e}",
                        );
                    }
                    continue;
                }
            };
            drop(client);

            // A completed `workspace/diagnostic` is one served request —
            // strike-ledger credit (misc 167).
            if let Some(key) = &served_key {
                self.client_manager.record_server_service(key);
            }

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
    ///
    /// `stuck_servers` names the diagnostic servers process-state evidence types
    /// as terminally wedged (respawn-dead / init-hung), each with its typed
    /// cause (misc 160 leg 1 / misc 167) — a `NoResults` file owed entirely by
    /// them renders the cause's wording: `[unverified — <server> stuck; will
    /// retry on demand]` while revivable, or the terminal `[broken — …]` /
    /// `[unstable — …]` labels once struck out.
    #[allow(
        clippy::too_many_arguments,
        reason = "one linear renderer over the distinct receipt inputs (covered paths, results, uncovered, out-of-scope, per-file servers, stuck servers, collapse flag)"
    )]
    fn format_output(
        &self,
        canonical_paths: &[PathBuf],
        file_results: &BTreeMap<String, (String, Vec<DiagEntry>)>,
        uncovered: &[UncoveredEntry],
        out_of_scope: &[OutOfScopeEntry],
        path_servers: &BTreeMap<String, BTreeSet<String>>,
        stuck_servers: &BTreeMap<String, UnverifiedCause>,
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
                        // The file's cause is the SOFTEST among its owing
                        // servers (`min`): "stuck" / "benched" are claims
                        // about a process, made only when every owing server
                        // earns them — a single still-alive owner that merely
                        // returned nothing keeps the softer "returned no
                        // result" wording (misc 160 leg 1 / misc 167).
                        let cause = servers
                            .iter()
                            .map(|s| {
                                stuck_servers
                                    .get(s)
                                    .copied()
                                    .unwrap_or(UnverifiedCause::Silent)
                            })
                            .min()
                            .unwrap_or(UnverifiedCause::Silent);
                        unverified_files.push(UnverifiedEntry {
                            display,
                            root,
                            server,
                            cause,
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
            served: canonical_paths.to_vec(),
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
    ///
    /// Returns [`BatchFault::RespawnDead`] when the server is **respawn-dead**:
    /// it died (terminal lifecycle) with a non-empty unretrieved remainder and
    /// the one bounded, strike-gated revive could not bring it back this run —
    /// process-state evidence the server is stuck, so its `NoResults` files earn
    /// the typed unverified wording (`stuck; will retry on demand`, or the
    /// terminal benched labels — misc 160 leg 1 / misc 167).
    ///
    /// Returns [`BatchFault::ContractViolation`] when the server stayed **alive**
    /// but its **verified discipline owed an answer this round and none came**
    /// ([`LspServer::owes_answer`]) — a declared-push server that never published
    /// or a debounce server whose version echo never landed inside the declared
    /// bound (diagnostics-debt 05). The remainder is the same unretrieved set the
    /// respawn-dead path reads; here the server is alive, so the fault is the
    /// contract, not the process. It feeds the same strike ledger a crashing
    /// server does.
    ///
    /// Returns `None` for a clean run, an alive server that owes no contract
    /// (its silence is the misc-153 residual, not a fault), or a revive that
    /// recovered the whole remainder.
    async fn run_server_batch_with_recovery(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        parent_id: Option<&str>,
        owner: Option<&str>,
        feeds: &mut BTreeMap<String, FileFeed>,
    ) -> Option<BatchFault> {
        let saved_this_round = self
            .run_server_batch(client_mutex, paths, parent_id, owner, feeds)
            .await;

        let remainder: Vec<PathBuf> = paths
            .iter()
            .filter(|p| !feeds.contains_key(p.to_string_lossy().as_ref()))
            .cloned()
            .collect();
        if remainder.is_empty() {
            return None;
        }

        let key = {
            let client = client_mutex.lock().await;
            // Recover only from an actual death; an alive, non-terminal server
            // is not an unavailability.
            if client.is_alive() && !client.lifecycle().is_terminal() {
                // Alive but with an unretrieved remainder: a verified-contract
                // violation iff the server's discipline OWED an answer this round.
                // Three arms:
                //   • the static owed-answer contract — declared-push (misc 187)
                //     or debounce (diagnostics-debt 05): a stimulated round owes a
                //     per-round response unconditionally;
                //   • the DIFF arm (misc 196): a diff server owes a publish on any
                //     round that delivered its save TRIGGER — our lifecycle sends
                //     `didSave` for changed files, so an alive diff server silent
                //     after that delivered save violates its contract; a diff round
                //     that delivered NO save owes nothing (silence is not a fault).
                // An alive server that owes no contract on this round (the
                // rust-analyzer never-republishes residual, an undeclared-silent
                // server, or a diff server that got no trigger) is left `None` —
                // its silence is not a fault. The scan arm lives at the
                // workspace-pull seam (`workspace_pull`), not here: scan servers
                // answer a whole-workspace pull, not a per-file batch. A server
                // violating its adapter is sick the same way a crashing one is, so
                // it feeds the SAME strike ledger.
                let owes = client.server().owes_answer()
                    || (client.server().is_diff() && saved_this_round);
                if owes {
                    if let Some(key) = client.server().key() {
                        drop(client);
                        self.client_manager.record_contract_violation(&key);
                    }
                    return Some(BatchFault::ContractViolation);
                }
                return None;
            }
            client.server().key()
        };
        let Some(key) = key else {
            // Terminal, but no respawnable key (a `SingleFile` scope): the
            // remainder degrades against a dead server — respawn-dead.
            return Some(BatchFault::RespawnDead);
        };

        // One bounded, strike-gated revive + re-run of the remainder (misc
        // 167). Any file still unretrieved afterwards stays `NoResults`
        // (degrade — no further attempts this run; the next demand retries,
        // strikes permitting).
        if let Some(fresh) = self.client_manager.revive_server(&key).await {
            // The revive is for a DEAD server (the alive-silent diff/contract arms
            // already resolved above), so its own save-trigger return is not a
            // fault input here — the respawn-dead verdict is the remainder check.
            let _ = self
                .run_server_batch(&fresh, &remainder, parent_id, owner, feeds)
                .await;
            // Respawn-dead iff any of the remainder is *still* unretrieved after
            // the fresh instance ran: the process-state evidence that the server
            // stays stuck across a recovery attempt.
            remainder
                .iter()
                .any(|p| !feeds.contains_key(p.to_string_lossy().as_ref()))
                .then_some(BatchFault::RespawnDead)
        } else {
            // Respawn itself failed to produce a live instance — dead again.
            Some(BatchFault::RespawnDead)
        }
    }

    /// Runs the batched diagnostics lifecycle on a single server — the
    /// **held-open batch round** (diagnostics-debt 01).
    ///
    /// Documents are opened once per connection and stay open across rounds:
    /// the round sends `didOpen` only for files not yet held, `didChange`
    /// (full sync, version++) only for files whose disk content moved since
    /// the last send (mtime fast-path, content hash breaking the same-mtime
    /// tie — this round-start check is also the out-of-band-write detection:
    /// servers do not deliver watched-files for open documents), and
    /// `didSave` for exactly the files whose last-sent content is unsaved.
    /// A file unchanged since its last saved send gets **no sync traffic**
    /// and serves from the push cache. Documents are never closed here —
    /// batch end is the owning agent's Stop/SubagentStop
    /// ([`LspClientManager::close_agent_docs`]); daemon death closes
    /// implicitly (bug 79, unchanged).
    ///
    /// Only blessed (diagnostics-eligible) servers reach this batch: an
    /// enrichment-only (unverified custom) server never serves diagnostics —
    /// [`LspServer::supports_diagnostics`] is always `false` for it, so
    /// server selection ([`LspClientManager::diagnostic_servers`]) filters it
    /// out upstream. Every server here gets this held-open batch lifecycle
    /// uniformly.
    ///
    /// Returns whether a `didSave` was delivered to the server this round — the
    /// diff floor arm's trigger signal (misc 196). `false` on any early bail
    /// (server dead, no files opened, settle failed): a round that delivered no
    /// save trigger leaves a diff server owing nothing.
    async fn run_server_batch(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        parent_id: Option<&str>,
        owner: Option<&str>,
        feeds: &mut BTreeMap<String, FileFeed>,
    ) -> bool {
        let Some(baseline) = self.pre_open_settle(client_mutex).await else {
            return false;
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
        // this gap widens. Drain the pipe first so the per-send cache
        // clears inside `open_document_on` see (and remove) those stale
        // entries rather than racing them. An **unchanged** held-open
        // file's cache entry deliberately survives — its last publish is
        // still evidence for the text the server holds, and the server
        // will not re-publish an unchanged document.
        {
            let server = client_mutex.lock().await.server().clone();
            drain_pipe(&server).await;
        }

        let docs = self.open_files(client_mutex, paths, parent_id, owner).await;
        if docs.is_empty() {
            return false;
        }

        // Settle + save + retrieve. Any bail → skip retrieve.
        let mut saved_this_round = false;
        if let Ok(stimulus) = self.settle_and_save(client_mutex, &docs, baseline).await {
            saved_this_round = stimulus.saved;
            // Drain any in-flight publishDiagnostics still in the stdio
            // pipe buffer before reading the diagnostic cache.
            let server = client_mutex.lock().await.server().clone();
            drain_pipe(&server).await;

            // Retrieval evidence bar (bug 99 residual / bug 101 / misc 156).
            // The activity-based settle cannot see work that has not started:
            // a silent diagnostic debounce timer (no CPU, no children, no
            // progress bracket) samples as idle while the publish the didSave
            // will produce is still pending. For never-heard files on a server
            // that has demonstrably pushed on this connection — and has no
            // working request channel to ask instead — hold retrieval until
            // that publish lands. URIs whose evidence never arrives come back
            // in `evidence_expired` and must not render `[clean]`.
            let evidence_expired = await_publish_evidence(client_mutex, &docs, stimulus).await;

            self.retrieve_diagnostics(client_mutex, &docs, feeds, &evidence_expired)
                .await;
        }

        // End of round: documents stay open (the held-open lifecycle); only
        // the causation scope is cleared.
        client_mutex.lock().await.set_parent_id(None);
        saved_this_round
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
        let result = await_idle(&server, detector, CancellationToken::new(), &server_name).await;
        debug!(
            server = %server_name,
            "batch pre-open idle result: {result:?}",
        );
        if result == SettleResult::RootDied {
            return None;
        }

        Some(sample_baseline(&server).await)
    }

    /// Syncs all batch files onto the server through the held-open change
    /// gate, collecting their URIs and per-file sync actions.
    ///
    /// Per file (via [`LspClientManager::open_document_on`]): `didOpen` when
    /// not yet held open, `didChange` (full text, version++) when disk
    /// content moved since the last send, **nothing** when unchanged. Each
    /// file is tagged with the owning `(session, agent)` editing key so
    /// Stop/SubagentStop can close exactly this agent's documents.
    /// Files that fail to open are logged and skipped.
    async fn open_files(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        parent_id: Option<&str>,
        owner: Option<&str>,
    ) -> Vec<RoundDoc> {
        let mut docs: Vec<RoundDoc> = Vec::new();

        for path in paths {
            match self
                .client_manager
                .open_document_on(path, client_mutex, parent_id.map(str::to_string), owner)
                .await
            {
                Ok((uri, action)) => docs.push(RoundDoc {
                    path: path.clone(),
                    uri,
                    synced: action.sends(),
                }),
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

        docs
    }

    /// Settles after the round's sync traffic, runs the health probe, and
    /// sends `didSave` for exactly the files that owe one.
    ///
    /// The save set is the registry's unsaved files
    /// ([`LspClient::document_needs_save`]): files this round `didOpen`ed or
    /// `didChange`d, plus files whose content an out-of-round send (a query
    /// open or the watched-files didChange relay) synced without a save —
    /// an on-save analyzer has not re-checked those. A file unchanged since
    /// its last saved send gets no `didSave` — no sync traffic at all
    /// (diagnostics-debt 01).
    ///
    /// Returns `Ok(RoundStimulus)` when the server is ready for retrieval — its
    /// `stimulated` is `true` when the round sent any traffic (sync or save), the
    /// retrieval evidence bar's arming input, and its `saved` is `true` when a
    /// `didSave` was delivered this round (the diff floor arm's trigger, misc 196)
    /// — or `Err(())` if the server died or a critical step failed (caller skips
    /// retrieval).
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
        docs: &[RoundDoc],
        post_open_baseline: u64,
    ) -> Result<RoundStimulus, ()> {
        let mut client = client_mutex.lock().await;

        if matches!(
            client.lifecycle(),
            ServerLifecycle::Failed | ServerLifecycle::Dead
        ) {
            return Err(());
        }

        let server = client.server().clone();
        let server_name = client.server_name().to_string();
        let cancel = CancellationToken::new();
        let synced_any = docs.iter().any(|d| d.synced);

        if synced_any
            && (!settle_after(
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
                ))
        {
            return Err(());
        }

        // ── Health probe ──────────────────────────────────────────
        if client.lifecycle() == ServerLifecycle::Probing
            && !client.run_health_probe(&docs[0].uri).await
        {
            return Err(());
        }

        // ── didSave the unsaved ───────────────────────────────────
        let mut saved_any = false;
        if client.wants_did_save() {
            let save_set: Vec<&str> = docs
                .iter()
                .filter(|d| client.document_needs_save(&d.uri))
                .map(|d| d.uri.as_str())
                .collect();
            if !save_set.is_empty() {
                let baseline = sample_baseline(&server).await;

                for uri in save_set {
                    if let Err(e) = client.did_save(uri).await {
                        warn!(
                            server = %server_name,
                            "batch didSave failed: {e}",
                        );
                        return Err(());
                    }
                    client.mark_document_saved(uri);
                    saved_any = true;
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
        }

        Ok(RoundStimulus {
            stimulated: synced_any || saved_any,
            saved: saved_any,
        })
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
    /// survives the merge — with two deliberate exceptions, both of which leave
    /// the file **unrecorded** so it resolves [`FileOutcome::NoResults`] and
    /// renders the honest `[unverified — <server> returned no result]` line
    /// instead of an absence-of-evidence `[clean]`:
    ///
    /// - a URI in `evidence_expired` (the retrieval evidence bar armed for it
    ///   and its publish never arrived, [`await_publish_evidence`]) whose
    ///   best-effort probe also goes unanswered (bug 99 residual / misc 156);
    /// - a **pull that errored** on a pull-discipline server ([`pull_settling`]
    ///   returning [`PullSettlement::Unsettled`]) — the pull's fault never
    ///   fabricates a clean; the debt stays unsettled (bug 84).
    async fn retrieve_diagnostics(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        docs: &[RoundDoc],
        feeds: &mut BTreeMap<String, FileFeed>,
        evidence_expired: &HashSet<String>,
    ) {
        let client = client_mutex.lock().await;

        let server_command = client.server_command().to_string();
        let server_name = client.server_name().to_string();
        let server_version = client.server_version().map(str::to_string);
        let lang_id = client.language().to_string();
        let has_code_actions = client.supports_code_action();
        // Strike-ledger credit target (misc 167): each file this retrieval
        // records is one delivered diagnostic verdict — served work.
        let served_key = client.server().key();

        let ctx = Arc::new(FeederContext {
            command: server_command,
            version: server_version,
            language_id: lang_id,
        });

        for RoundDoc { path, uri, .. } in docs {
            let diagnostics = match client.settled_diagnostics(uri) {
                // A publish SETTLED the debt for this file — authoritative, EVEN
                // WHEN EMPTY. Settlement (not mere presence, bug 85) means a
                // versioned publish echoed the current version — `Some(vec![])`
                // is then the versioned-empty authoritative clean — or a
                // discipline-trusted hint carried it (an unversioned non-empty
                // publish's real findings; a declared-push server's contractual
                // empty). Pull is never consulted, so a push+pull server reports
                // one source per file per run (no double-reporting) and a
                // push-only server's honest empty is never second-guessed by an
                // off-spec probe (misc 153). A heard-but-non-settling publish (an
                // unversioned empty from an undeclared server, a stale version)
                // reads `None` here and falls to the never-heard path — the debt
                // stays unsettled until a current-version echo lands.
                Some(diags) => diags,
                // Never heard AND the server advertises the pull model: ask it
                // the way it asked to be asked. A returned report settles the
                // debt (even empty — the on-demand clean); an error leaves it
                // UNSETTLED — skip recording so the file resolves NoResults and
                // renders the honest `[unverified — …]` line, never a fabricated
                // `[clean]` (bug 84). ServerCancelled is re-triggered, and only
                // -32601 downgrades the capability (see `pull_settling`).
                None if client.supports_pull_diagnostics() => {
                    match pull_settling(&client, uri).await {
                        PullSettlement::Settled(diags) => diags,
                        PullSettlement::Unsettled => continue,
                    }
                }
                // Never heard, pull suppressed by engine casing (misc 157): the
                // server is push-first by design (rust-analyzer) and must never be
                // sent `textDocument/diagnostic`, not even the best-effort probe —
                // its native pushes are the sole channel. Never-heard resolves the
                // same as a genuinely silent push-only server — even when the
                // evidence bar armed and expired for this URI. That is a stated
                // residual (bug 99 residual / misc 156): rust-analyzer does not
                // re-publish an unchanged result, so a repeat-clean file is
                // routinely never-heard here and forcing `[unverified]` would
                // misfire on every such run. The bar's wait above still holds
                // retrieval through any late reaction it can observe (bug 101's
                // didSave-mediated window); a publish silent past the dead-air
                // budget with no process activity remains invisible.
                None if client.server().pull_suppressed() => Vec::new(),
                // Never heard and no advertised pull capability. Some servers
                // answer `textDocument/diagnostic` on demand without
                // advertising it (the bug-74 shape — lattice did at the time,
                // before going push-only; today it rejects the probe with
                // `-32601`): ask directly rather than report a false `[clean]`
                // for a fast publisher whose first publish the
                // settle-then-collect pipeline cleared (bug 74).
                None => {
                    match client.try_pull_diagnostics(uri).await {
                        // The server ANSWERED the probe — evidence computed on
                        // demand, even when empty. A publish that raced the
                        // probe still outranks an empty answer (bug 99; one
                        // source per file, misc 153).
                        Some(pulled) => reconsult_push_after_empty_pull(&client, uri, pulled),
                        // The probe went unanswered (`-32601` and friends) —
                        // that is not evidence of anything. Re-consult the push
                        // cache for a SETTLING publish: one that landed during the
                        // round-trip and echoes the current version is evidence in
                        // hand (bug 99); a stale or non-settling one is not.
                        None => match client.settled_diagnostics(uri) {
                            Some(published) => published,
                            // No publish, no answered probe. If the evidence
                            // bar armed for this URI and expired, the pipeline
                            // has NO evidence the server reacted — the file
                            // must not render `[clean]`. Skip recording so it
                            // resolves `NoResults` → the honest
                            // `[unverified — <server> returned no result]`.
                            None if evidence_expired.contains(uri) => continue,
                            // Bar unarmed (the server never pushed on this
                            // connection): the misc-153 silent-server contract
                            // holds — the probe was the one best effort and
                            // absence resolves clean.
                            None => Vec::new(),
                        },
                    }
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

            // Served work pays the strike counter down (misc 167): one
            // delivered per-file verdict (clean or dirty) is one completed
            // request. Spawn/initialize and the eager probe earn nothing —
            // only this demand-side delivery does.
            if let Some(key) = &served_key {
                self.client_manager.record_server_service(key);
            }
        }
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
    let result = await_idle(server, detector, cancel, server_name).await;
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
/// Requests a reader-side barrier (`Connection::drain`): the reader
/// loop consumes and dispatches frames until the pipe is momentarily
/// empty, then acks — so every byte the server wrote before this call,
/// including any final `publishDiagnostics`, has been processed. (The
/// former design injected a sentinel frame into the pipe's write end;
/// a second writer on the pipe corrupted mid-write server frames —
/// the bug-95 incident.)
///
/// Without this, `retrieve_diagnostics` can read stale diagnostics
/// from an intermediate analysis phase (e.g., rust-analyzer's
/// fast-check results that are later clobbered by fly-check).
async fn drain_pipe(server: &LspServer) {
    if let Err(e) = server.drain().await {
        debug!("drain_pipe: {e}");
    }
}

/// Dead-air budget for the retrieval evidence bar, in settle-cadence poll
/// samples ([`POLL_INTERVAL`], 50 ms): the amount of *observed quiet* —
/// samples with no CPU/page-fault delta, no pending-work scheduler state, and
/// no open progress bracket — [`await_publish_evidence`] tolerates before
/// concluding a declared- or demonstrated-push server has nothing to say for a
/// batch's never-heard files.
///
/// A work count over quiet samples, never elapsed wall-clock time (the
/// declared, evidence-anchored floor form the contention doctrine exempts):
/// activity never consumes it, so a server visibly reacting late — a flycheck
/// child spawning after settle sampled idle, bug 101's window — extends the
/// wait for exactly as long as the reaction lasts, unbounded, the same stance
/// the settle loop takes (bug 24/28). Only dead air drains it.
///
/// Anchoring: the pure-debounce dead zone the incidents measured is ~270 ms
/// (lua-language-server, conformance run 29067405830) to ~300-400 ms
/// (yaml-language-server, run 29068921605) of scheduler-invisible timer sleep
/// between the didSave and the publish. 30 quiet samples ≈ 1.5 s of observed
/// dead air ≥ 3.7× the worst measured zone — margin for slower debouncers —
/// while keeping the fallback (the bug-74 best-effort probe) reachable
/// without pathological latency for a push server that will never re-publish
/// an unchanged document.
const DEBOUNCE_DEAD_ZONE_SAMPLES: u32 = 30;

/// The dead-air budget in poll samples for the batch's owing server — the
/// declared-constant gate (diagnostics-debt 05).
///
/// A debounce-discipline server declares its window in the manifest
/// ([`LspServer::debounce_ms`], data riding the pin — never a measured guess).
/// For such a server the evidence bar awaits the version echo bounded by that
/// declared constant: the window converted to [`POLL_INTERVAL`]-cadence samples
/// (rounded up) PLUS the generic [`DEBOUNCE_DEAD_ZONE_SAMPLES`] slack for the
/// publish to land and be read after the timer fires. The wait is still
/// **arrival-based** — the loop returns the moment the echo lands inside the
/// bound; the bound only caps how long a *silent* server holds collection before
/// its expiry renders the fault-attribution wording (never `[clean]`). Every
/// other server keeps the generic budget: the declared constant governs only the
/// discipline that reads it.
fn debounce_budget_samples(server: &LspServer) -> u32 {
    let poll_ms = u32::try_from(POLL_INTERVAL.as_millis())
        .unwrap_or(50)
        .max(1);
    server
        .debounce_ms()
        .map_or(DEBOUNCE_DEAD_ZONE_SAMPLES, |window_ms| {
            // ceil(window_ms / poll_ms), saturating — the declared window in samples.
            let window_ms = u32::try_from(window_ms).unwrap_or(u32::MAX);
            let window_samples = window_ms.div_ceil(poll_ms);
            window_samples.saturating_add(DEBOUNCE_DEAD_ZONE_SAMPLES)
        })
}

/// Holds retrieval until a declared- or demonstrated-push server's publishes
/// arrive for the batch's never-heard URIs, or the dead-air budget drains —
/// the retrieval evidence bar (bug 99 residual / bug 101 / misc 156 / misc
/// 187).
///
/// The activity-based settle cannot see work that has not started (a silent
/// debounce timer, VFS latency), so "settled + cache empty" is absence of
/// evidence, not evidence of clean. The bar arms only when the incident
/// signature is complete:
///
/// - at least one opened URI is **never-heard** after the post-didSave settle
///   and drain (no cached publish, not even an empty one);
/// - the server is a push server by **declaration, demonstration, OR debounce
///   discipline**: either its conformance profile carries the publish contract
///   ([`LspServer::declares_push`] — a publish on every didOpen, explicit
///   `[]` for clean, misc 187), or it **has published** on this connection
///   ([`LspServer::has_ever_published`]), or its manifest discipline is
///   `debounce` with a declared bound ([`LspServer::debounce_ms`],
///   diagnostics-debt 05 — the version echo is owed within the declared window,
///   so the gate awaits it from turn zero). Either way, the didOpen/didSave it
///   just received will produce a publish (possibly empty — the heard-empty
///   clean, misc 153). Demonstration alone left a first-run false-`[clean]`
///   window on every fresh connection, since `has_ever_published` resets with
///   the connection; the declaration and the declared debounce bound each close
///   it from turn zero;
/// - it advertises **no pull channel** and has **never answered a probe**
///   ([`LspServer::has_answered_probe`]) — a working request channel is
///   per-file evidence on demand, so no wait is owed where one exists.
///
/// A server that neither declares push, has ever pushed, nor declares a debounce
/// bound is left untouched to the misc-153 silent-server contract downstream
/// (one best-effort probe → clean).
///
/// The wait wakes on every publish ([`LspServer::diagnostics_notify`]
/// registered before each cache re-check, so no publish is missed) and never
/// consumes budget while the server shows work — an open `Busy` bracket or a
/// working tree ([`tree_working`]). Only quiet samples drain
/// [`DEBOUNCE_DEAD_ZONE_SAMPLES`]. Liveness is owned by cancel-on-disconnect
/// exactly as for the settle loop (bug 24): a server that works forever
/// without publishing holds this wait just as it would have held settle.
///
/// Returns the URIs still never-heard after the budget drained (following a
/// final reader-side drain + re-check, so a publish written inside the last
/// poll window is not dropped). The caller must not let those render
/// `[clean]` from absence. An empty set means every URI was heard or the bar
/// never armed.
///
/// The **diff floor arm** (misc 196) also arms the bar: a
/// [`diff`-discipline](crate::recipes::Discipline::Diff) server owes a publish on
/// any round that delivered its save trigger (`stimulus.saved`). A diff server
/// declares no push and may not have published on this connection, so without this
/// arm its never-heard file would fall through to the misc-153 silent residual and
/// render `[clean]` over a delivered-but-unanswered save. Arming holds collection,
/// and on expiry the file returns never-heard (into `evidence_expired`), so it
/// stays absent from `feeds` and the per-file batch's contract-violation arm
/// (`run_server_batch_with_recovery`) attributes the fault. A diff round that
/// delivered NO save does not arm here — silence is not owed, so not a fault.
///
/// `stimulus` carries whether the round sent the server any traffic (`stimulated`)
/// and whether a `didSave` was delivered (`saved`). An **unstimulated** round
/// (every file unchanged and saved — the held-open repeat run) owes the server no
/// wait: nothing was sent, so no publish is coming, and the still-never-heard set
/// expires immediately — keeping a file that expired `[unverified]` last round
/// honestly `[unverified]` on the repeat run instead of flipping to a false
/// `[clean]` (diagnostics-debt 01).
async fn await_publish_evidence(
    client_mutex: &Arc<Mutex<LspClient>>,
    docs: &[RoundDoc],
    stimulus: RoundStimulus,
) -> HashSet<String> {
    let (server, server_name, mut pending) = {
        let client = client_mutex.lock().await;
        // An advertised pull provider is asked directly at retrieval — the
        // pull response is its own per-file evidence.
        if client.supports_pull_diagnostics() {
            return HashSet::new();
        }
        let pending: HashSet<String> = docs
            .iter()
            .filter(|d| client.get_diagnostics(&d.uri).is_none())
            .map(|d| d.uri.clone())
            .collect();
        (
            client.server().clone(),
            client.server_name().to_string(),
            pending,
        )
    };

    // Declaration OR demonstration OR debounce discipline: a declared-push
    // server (misc 187) and a debounce-discipline server (diagnostics-debt 05)
    // each arm the bar even before this connection's first publish —
    // `has_ever_published` is per-connection state that resets on every respawn
    // and daemon bounce, which is exactly the first-run false-`[clean]` window
    // both close. The debounce bound is what the gate awaits (data riding the
    // pin), so a debounce server owes an echo within it from turn zero.
    // The diff floor arm (misc 196): a diff-discipline server owes a publish on a
    // round that delivered its save trigger, so arm the bar for it too — otherwise
    // its never-heard file falls through to the misc-153 silent residual and reads
    // `[clean]` over a delivered-but-unanswered save. A diff round with NO delivered
    // save does not owe an answer, so it does not arm here.
    let diff_owes_this_round = server.is_diff() && stimulus.saved;
    if pending.is_empty()
        || !(server.declares_push()
            || server.has_ever_published()
            || server.debounce_ms().is_some()
            || diff_owes_this_round)
        || server.has_answered_probe()
    {
        return HashSet::new();
    }

    // No stimulus this round → no publish is owed or coming; the never-heard
    // set expires without a wait (see the doc comment).
    if !stimulus.stimulated {
        debug!(
            server = %server_name,
            pending = pending.len(),
            "evidence bar: unstimulated round, never-heard files expire without a wait",
        );
        return pending;
    }

    // The bound: the declared debounce constant (converted to a sample budget)
    // for a debounce-discipline server, else the generic dead-air budget
    // (diagnostics-debt 05). Data riding the pin, never a measured guess; the
    // window itself never surfaces to the agent — the gate holds collection.
    let budget = debounce_budget_samples(&server);
    debug!(
        server = %server_name,
        pending = pending.len(),
        budget,
        debounce_ms = server.debounce_ms(),
        "evidence bar armed: never-heard files on a declared- or demonstrated-push server",
    );

    let mut quiet_samples: u32 = 0;
    loop {
        // Register the publish wakeup BEFORE re-checking the cache so a
        // publish dispatched between the check and the select is never missed.
        let notified = server.diagnostics_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        retain_unheard(&server, &mut pending);
        if pending.is_empty() {
            debug!(server = %server_name, "evidence bar: all publishes heard");
            return HashSet::new();
        }
        // A terminal server produces no more publishes; the recovery path
        // (run_server_batch_with_recovery) owns what happens next.
        if server.lifecycle().is_terminal() {
            return pending;
        }
        if quiet_samples >= budget {
            break;
        }

        tokio::select! {
            () = &mut notified => continue,
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        // Work accounting: an open progress bracket (`Busy`), an announced
        // work-done token not yet begun (`Pending` — the elm cold-download
        // gap, misc 200), or a working tree is the server still reacting — the
        // budget is untouched (work-based; never cap observed work). Only a
        // quiet sample drains it. A created-never-begun token rides the same
        // Stuck ceiling as a hung `Busy` bracket — no new clock.
        if matches!(
            server.lifecycle(),
            ServerLifecycle::Busy(_) | ServerLifecycle::Pending(_)
        ) {
            continue;
        }
        let sampler = Arc::clone(&server);
        match tokio::task::spawn_blocking(move || sampler.sample_tree())
            .await
            .ok()
            .flatten()
        {
            // Tree gone — the root died; the terminal check above and the
            // recovery path own it.
            None => return pending,
            Some(snapshot) if tree_working(&snapshot) => {}
            Some(_) => quiet_samples = quiet_samples.saturating_add(1),
        }
    }

    // Budget drained with unheard URIs. One reader-side barrier + final
    // re-check: a publish whose bytes were written inside the last poll
    // window may not have been dispatched yet.
    drain_pipe(&server).await;
    retain_unheard(&server, &mut pending);
    if !pending.is_empty() {
        debug!(
            server = %server_name,
            pending = pending.len(),
            quiet_samples,
            "evidence bar expired: publish never arrived",
        );
    }
    pending
}

/// Drops every URI the server's push cache now holds an entry for (heard —
/// including heard-empty) from `pending`, leaving only the still-never-heard.
fn retain_unheard(server: &LspServer, pending: &mut HashSet<String>) {
    let cache = server
        .diagnostics
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.retain(|uri| !cache.contains_key(uri));
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
/// the same finding reported by two different sources collapses: bash-language-server's
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

/// Resolves an empty pull against the push cache one more time (bug 99).
///
/// The pull's round-trip is a wait the settle phase never granted this
/// server: a debouncing push-only server (the incident's brew
/// lua-language-server publishes ~270 ms after `didSave` while settle sees
/// idle at 60 ms) can publish while the pull is in flight, and the reader
/// dispatches that publish into the push cache strictly before it resolves
/// the pull's response. A **settling** publish in hand — one echoing the
/// current version ([`LspClient::settled_diagnostics`]) — outranks the probe's
/// nothing; rendering `[clean]` over a cached truthful publish would falsify
/// the receipt. A non-empty pull is returned as-is (one source per file, misc
/// 153), and a cache with no settling publish keeps the pull's honest empty. A
/// stale or non-settling cached publish is ignored — it never overrides the
/// pull's fresh answer (diagnostics-debt 03).
fn reconsult_push_after_empty_pull(
    client: &LspClient,
    uri: &str,
    pulled: Vec<Value>,
) -> Vec<Value> {
    if pulled.is_empty()
        && let Some(published) = client.settled_diagnostics(uri)
    {
        return published;
    }
    pulled
}

/// Pulls diagnostics from a pull-discipline server, settling the debt honestly
/// on every outcome (bug 84).
///
/// The pull is the server's on-demand verdict, so a returned report — empty or
/// not — [`settles`](PullSettlement::Settled) the debt (re-consulting the push
/// cache first, bug 99: a publish that raced the round-trip is evidence in
/// hand). Errors never fabricate a clean:
///
/// - **`ServerCancelled` (-32802)** is the spec's "busy, re-trigger": the same
///   request is re-issued, bounded by [`PULL_RETRIGGER_LIMIT`], while the
///   server's `DiagnosticServerCancellationData.retriggerRequest` permits it
///   (absent data defaults to retrigger). An exhausted or declined re-trigger
///   leaves the debt [`Unsettled`](PullSettlement::Unsettled) — the fault named
///   in the log, never a silent zero.
/// - **`MethodNotFound` (-32601)** is the *only* evidence that downgrades the
///   capability permanently ([`LspServer::downgrade_pull_diagnostics`]): the
///   method is genuinely unsupported. Even so, this round's debt is left
///   unsettled — a downgrade governs future rounds, it does not fabricate a
///   verdict for this one.
/// - **Any other error** (a transient `InternalError`, a transport fault)
///   downgrades *nothing* and leaves the debt unsettled — the next round pulls
///   again.
///
/// On any error the push cache is re-consulted once more for a **settling**
/// publish: one that landed during the failing round-trip and echoes the
/// current version settles the debt legitimately (bug 99). A stale or
/// non-settling cached publish must not settle a fresh debt here either — the
/// re-consult is version-aware ([`LspClient::settled_diagnostics`]), so an old
/// round's straggler never fabricates this round's verdict (diagnostics-debt 03).
async fn pull_settling(client: &LspClient, uri: &str) -> PullSettlement {
    let mut retriggers = 0u32;
    loop {
        match client.pull_diagnostics(uri).await {
            Ok(diags) => {
                return PullSettlement::Settled(reconsult_push_after_empty_pull(
                    client, uri, diags,
                ));
            }
            Err(e) => {
                let code = e
                    .downcast_ref::<crate::lsp::connection::LspResponseError>()
                    .map(|r| r.code);

                // ServerCancelled: re-trigger per the spec, bounded — a busy
                // server gets a fresh request, never an infinite spin.
                if code == Some(LSP_SERVER_CANCELLED)
                    && retrigger_requested(&e)
                    && retriggers < PULL_RETRIGGER_LIMIT
                {
                    retriggers += 1;
                    debug!("pull diagnostics ServerCancelled, re-triggering ({retriggers})");
                    continue;
                }

                // MethodNotFound is the sole downgrade evidence (bug 84): the
                // method is genuinely absent, so future rounds skip the pull.
                if code == Some(LSP_METHOD_NOT_FOUND) {
                    client.server().downgrade_pull_diagnostics();
                    debug!("pull diagnostics unsupported (-32601), downgraded: {e}");
                } else {
                    // Transient (busy past the bound, internal error, transport
                    // fault): no downgrade — the next round retries the pull.
                    debug!("pull diagnostics failed (transient, no downgrade): {e}");
                }

                // Bug 99: a SETTLING publish that raced the failing round-trip
                // is evidence in hand and settles the debt legitimately; a stale
                // or non-settling one leaves the debt unsettled (bug 85 — a
                // stale-version publish never settles a fresh debt here either).
                return client
                    .settled_diagnostics(uri)
                    .map_or(PullSettlement::Unsettled, PullSettlement::Settled);
            }
        }
    }
}

/// Reads the LSP `DiagnosticServerCancellationData.retriggerRequest` hint off a
/// `ServerCancelled` error, defaulting to `true`.
///
/// The spec's data payload is `{ "retriggerRequest": bool }`. When the server
/// omits the data (or the flag), the safe default is to re-trigger — a bare
/// `ServerCancelled` is "busy, ask again". Only an explicit
/// `retriggerRequest: false` declines the re-issue.
fn retrigger_requested(err: &anyhow::Error) -> bool {
    err.downcast_ref::<crate::lsp::connection::LspResponseError>()
        .and_then(|r| r.data.as_ref())
        .and_then(|d| d.get("retriggerRequest"))
        .and_then(Value::as_bool)
        != Some(false)
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

/// The bracketed label of an unverified receipt line (without the brackets).
///
/// One label per [`UnverifiedCause`]: the gate-escape wording distinguishes a
/// stuck process from a server that was alive and simply produced nothing
/// (misc 160 leg 1), and the strike ledger's terminal states carry the
/// ticket's cause-distinguishing labels (misc 167) — `broken` (config /
/// environment: fix the server) vs `unstable` (instability, not config). The
/// verified-contract-violation arm renders the DESIGN's exact wording
/// (diagnostics-debt 05 / DESIGN §"The floor is fault attribution"): a blessed
/// server whose discipline owed a response this round and gave none is a fault,
/// attributed to the server, not a Catenary shrug.
fn unverified_label(server: &str, cause: UnverifiedCause) -> String {
    match cause {
        UnverifiedCause::Silent => {
            format!("unverified \u{2014} {server} returned no result")
        }
        UnverifiedCause::ContractViolation => {
            format!(
                "{server} did not answer for this round \u{2014} its verified behavior \
                 requires a response; treating as a server fault, re-run to retry"
            )
        }
        UnverifiedCause::Stuck => {
            format!("unverified \u{2014} {server} stuck; will retry on demand")
        }
        UnverifiedCause::BenchedBroken => {
            format!("broken \u{2014} {server} never started")
        }
        UnverifiedCause::BenchedUnstable => {
            format!("unstable \u{2014} {server} gave up after repeated crashes")
        }
    }
}

/// Formats the per-file receipt.
///
/// Bare root-path section headers. Every diagnosed file is listed: dirty files
/// with their diagnostics beneath, clean files with a `[clean]` line beside the
/// path, unverified files with an `[unverified — <server> returned no result]`
/// (or `stuck`) line, uncovered files noted with `[no LSP coverage]`. Clean is
/// **explicit**, never silence (workstream 37 ticket 01, retiring misc 111 /
/// decision 022) — the receipt is proof of the debt the run paid, so every file
/// it diagnosed appears and counts toward the collapse total. An `unverified`
/// file (its server died before producing a result) appears for the same reason
/// (bug 56): an all-`NoResults` set must never render as empty stdout.
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
    let mut root_unverified: BTreeMap<&PathBuf, Vec<(&str, &str, UnverifiedCause)>> =
        BTreeMap::new();
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
            .push((&ue.display, &ue.server, ue.cause));
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
                for (display, server, cause) in unv_files {
                    _ = writeln!(
                        output,
                        "{} [{}]",
                        root.join(display).display(),
                        unverified_label(server, *cause),
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
                    for (display, server, cause) in unv_files {
                        _ = writeln!(output, "\t{display} [{}]", unverified_label(server, *cause));
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

    /// Build a `returned no result` [`UnverifiedEntry`] (test ergonomics).
    fn ue(display: &str, root: &str, server: &str) -> UnverifiedEntry {
        ue_cause(display, root, server, UnverifiedCause::Silent)
    }

    /// Build a **stuck** [`UnverifiedEntry`] — the gate-escape wording (misc 160
    /// leg 1): process-state evidence types the owing server as terminally
    /// wedged, strikes remaining (misc 167).
    fn ue_stuck(display: &str, root: &str, server: &str) -> UnverifiedEntry {
        ue_cause(display, root, server, UnverifiedCause::Stuck)
    }

    /// Build an [`UnverifiedEntry`] with an explicit cause (misc 167).
    fn ue_cause(
        display: &str,
        root: &str,
        server: &str,
        cause: UnverifiedCause,
    ) -> UnverifiedEntry {
        UnverifiedEntry {
            display: display.to_string(),
            root: PathBuf::from(root),
            server: server.to_string(),
            cause,
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
    fn format_stuck_server_says_stuck_not_returned_no_result() {
        // The gate escape (misc 160 leg 1): a file whose owing server is
        // process-state-terminal (respawn-dead / init-hung) renders
        // `[unverified — <server> stuck; will retry on demand]` (misc 167 —
        // the strike gate retries dead servers on demand, so "stuck" is no
        // longer permanent), distinguishing a wedged process from an
        // alive-but-silent server. The banner still names it; the gate is
        // still paid (the receipt returns non-empty).
        let unverified = vec![ue_stuck("file.rs", "/test", "rust-analyzer")];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        assert_eq!(
            output.trim(),
            "unavailable: rust-analyzer\n\
             /test/file.rs [unverified \u{2014} rust-analyzer stuck; will retry on demand]",
            "output: {output}"
        );
        assert!(
            !output.contains("returned no result"),
            "a stuck server must not read as merely silent: {output}"
        );
    }

    #[test]
    fn format_contract_violation_renders_the_designs_exact_wording() {
        // The fault floor's third arm (diagnostics-debt 05 / DESIGN §"The floor
        // is fault attribution"): a blessed server whose verified discipline owed
        // an answer this round and gave none renders the DESIGN's exact wording —
        // a server fault, attributed to the server, never a Catenary shrug and
        // never a false `[clean]`. The retired v1 "publishes on its own schedule"
        // wording stays unwritten.
        let unverified = vec![ue_cause(
            "case.ts",
            "/project",
            "typescript-language-server",
            UnverifiedCause::ContractViolation,
        )];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        assert_eq!(
            output.trim(),
            "unavailable: typescript-language-server\n\
             /project/case.ts [typescript-language-server did not answer for this round \u{2014} \
             its verified behavior requires a response; treating as a server fault, \
             re-run to retry]",
            "output: {output}"
        );
        assert!(
            !output.contains("publishes on its own schedule"),
            "the retired v1 shrug must never be written: {output}"
        );
        assert!(
            !output.contains("[clean]"),
            "a fault is never clean: {output}"
        );
    }

    #[test]
    fn contract_violation_ordering_between_silent_and_stuck() {
        // The softest-`min` ordering is load-bearing (diagnostics-debt 05): a
        // contract violation is harder than a plain silence (the discipline is
        // the evidence) but softer than a process-state death (a terminal
        // lifecycle is harder evidence still). So a file owed by a silent co-owner
        // keeps `Silent`'s wording, while a stuck co-owner wins over a violation.
        assert!(UnverifiedCause::Silent < UnverifiedCause::ContractViolation);
        assert!(UnverifiedCause::ContractViolation < UnverifiedCause::Stuck);
        let with_silent = [UnverifiedCause::ContractViolation, UnverifiedCause::Silent]
            .into_iter()
            .min();
        assert_eq!(with_silent, Some(UnverifiedCause::Silent));
        let with_stuck = [UnverifiedCause::ContractViolation, UnverifiedCause::Stuck]
            .into_iter()
            .min();
        assert_eq!(with_stuck, Some(UnverifiedCause::ContractViolation));
    }

    #[test]
    fn debounce_budget_rides_the_declared_constant() {
        // The declared-constant gate (diagnostics-debt 05): a debounce server's
        // await is bounded by its declared window (converted to poll samples)
        // PLUS the generic dead-air slack — data riding the pin, never a hardcoded
        // number. `mockls-debounce` declares 300 ms; at the 50 ms poll cadence
        // that is 6 window samples + the generic slack. Every non-debounce server
        // keeps the generic budget. A pure state assertion — no wall clock.
        let debounce = LspServer::new(
            "mockls-debounce".to_string(),
            "mockls-debounce".to_string(),
            None,
        );
        let poll_ms = u32::try_from(POLL_INTERVAL.as_millis())
            .expect("poll interval fits u32")
            .max(1);
        let window_samples = 300u32.div_ceil(poll_ms);
        assert_eq!(
            debounce_budget_samples(&debounce),
            window_samples + DEBOUNCE_DEAD_ZONE_SAMPLES,
            "the declared 300 ms window must set the bound, not the hardcoded budget",
        );

        // A non-debounce server (rust-analyzer: event discipline, no bound) keeps
        // the generic dead-air budget — the declared constant governs only the
        // discipline that reads it.
        let event = LspServer::new("rust".to_string(), "rust-analyzer".to_string(), None);
        assert_eq!(
            debounce_budget_samples(&event),
            DEBOUNCE_DEAD_ZONE_SAMPLES,
            "a non-debounce server keeps the generic dead-air budget",
        );
    }

    #[test]
    fn format_stuck_and_silent_coexist_with_distinct_wording() {
        // A stuck server (evidence) and a plain-silent server (no evidence) in
        // the same run keep distinct wording — "stuck" is a claim made only on
        // process-state evidence, never blanket.
        let unverified = vec![
            ue_stuck("src/a.rs", "/project", "rust-analyzer"),
            ue("src/b.rs", "/project", "gopls"),
        ];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        assert!(
            output.contains(
                "\tsrc/a.rs [unverified \u{2014} rust-analyzer stuck; will retry on demand]"
            ),
            "output: {output}"
        );
        assert!(
            output.contains("\tsrc/b.rs [unverified \u{2014} gopls returned no result]"),
            "output: {output}"
        );
    }

    #[test]
    fn format_benched_broken_renders_ticket_label() {
        // Struck out with zero successes ever (misc 167): the terminal label
        // names the cause — config/environment, fix the server. The banner
        // still opens the receipt and the gate is still paid.
        let unverified = vec![ue_cause(
            "file.rs",
            "/test",
            "rust-analyzer",
            UnverifiedCause::BenchedBroken,
        )];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        assert_eq!(
            output.trim(),
            "unavailable: rust-analyzer\n\
             /test/file.rs [broken \u{2014} rust-analyzer never started]",
            "output: {output}"
        );
    }

    #[test]
    fn format_benched_unstable_renders_ticket_label() {
        // Struck out with prior successes (misc 167): instability, not config.
        let unverified = vec![ue_cause(
            "file.rs",
            "/test",
            "rust-analyzer",
            UnverifiedCause::BenchedUnstable,
        )];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        assert_eq!(
            output.trim(),
            "unavailable: rust-analyzer\n\
             /test/file.rs [unstable \u{2014} rust-analyzer gave up after repeated crashes]",
            "output: {output}"
        );
    }

    #[test]
    fn unverified_cause_min_keeps_the_softest_claim() {
        // A file owed by several servers renders the softest cause among them:
        // one alive-but-silent owner keeps the `Silent` wording even when the
        // other is benched (misc 160 leg 1 / misc 167).
        assert!(UnverifiedCause::Silent < UnverifiedCause::Stuck);
        assert!(UnverifiedCause::Stuck < UnverifiedCause::BenchedBroken);
        assert!(UnverifiedCause::BenchedBroken < UnverifiedCause::BenchedUnstable);
        let softest = [UnverifiedCause::BenchedUnstable, UnverifiedCause::Silent]
            .into_iter()
            .min();
        assert_eq!(softest, Some(UnverifiedCause::Silent));
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
            ue("a.jl", "/project", "julia-language-server"),
            ue("b.rs", "/project", "rust-analyzer"),
        ];
        let output = format_diagnostics(&[], &[], &[], &unverified, false);
        let julia = output
            .find("unavailable: julia-language-server")
            .expect("julia banner");
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

    // ── pull honesty: an error is never `Clean` (bug 84) ────────────────
    //
    // Operation/state assertions over `pull_settling`, no wall clocks. The
    // helper is the single seam the retrieval path routes a pull-discipline
    // server through: a returned report settles the debt, an error leaves it
    // unsettled, and only `-32601` downgrades the capability. Each test spawns
    // mockls and drives the pull directly, asserting the `PullSettlement`
    // variant and the server's `supports_pull_diagnostics` state.

    use crate::lsp::connection::LspResponseError;

    /// Spawns mockls with `extra_args`, initialized against a fresh tempdir.
    async fn spawn_pull_client(extra_args: &[&str]) -> (LspClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = crate::lsp::test_support::mockls_bin();
        let bin_str = bin.to_str().expect("mockls path is UTF-8");
        let lang = "pUll1";
        let mut args = vec![lang];
        args.extend_from_slice(extra_args);
        let mut client = LspClient::spawn(
            bin_str,
            &args,
            lang,
            lang,
            crate::logging::LoggingServer::new(),
            None,
            None,
            "",
        )
        .expect("spawn mockls");
        client
            .initialize(&[dir.path().to_path_buf()], None)
            .await
            .expect("initialize");
        (client, dir)
    }

    /// A never-opened URI reads never-heard, so the pull is the only evidence
    /// channel — no racing publish can settle the debt behind the pull's back.
    const UNOPENED_URI: &str = "file:///pull-honesty/never-opened.pUll1";

    /// A pull error leaves the debt UNSETTLED — never an empty-vec `Clean`
    /// placeholder (bug 84's retired mechanism). An `InternalError` (-32603)
    /// from `--fail-pull` returns `Unsettled` and, being transient, downgrades
    /// nothing: the next round pulls again.
    #[tokio::test]
    async fn pull_error_leaves_debt_unsettled_without_downgrade() {
        let (mut client, _dir) = spawn_pull_client(&["--pull-diagnostics", "--fail-pull"]).await;

        assert!(
            client.supports_pull_diagnostics(),
            "the server advertises pull before the first attempt"
        );

        let outcome = pull_settling(&client, UNOPENED_URI).await;
        assert!(
            matches!(outcome, PullSettlement::Unsettled),
            "a failed pull leaves the debt unsettled, never a Clean placeholder"
        );
        assert!(
            client.supports_pull_diagnostics(),
            "a transient pull failure must NOT downgrade the capability"
        );

        // Next round: the capability is intact, so the pull is attempted again
        // (and fails again, staying unsettled) — never a silent skip-to-clean.
        let again = pull_settling(&client, UNOPENED_URI).await;
        assert!(
            matches!(again, PullSettlement::Unsettled),
            "the next round retries the pull after a transient failure"
        );

        client.shutdown().await.expect("shutdown");
    }

    /// The pull error-path re-consult is version-aware (diagnostics-debt 03): a
    /// STALE cached publish (a previous round's straggler) must not settle a
    /// fresh debt on the error path either — the debt stays unsettled.
    #[tokio::test]
    async fn pull_error_stale_cached_publish_does_not_settle() {
        let (mut client, _dir) = spawn_pull_client(&["--pull-diagnostics", "--fail-pull"]).await;
        let uri = "file:///pull-honesty/stale.pUll1";

        // The debt is at version 3, but a version-2 publish (a straggler from a
        // previous round) sits in the cache — heard, but stale.
        client.server().note_doc_version(uri, 3);
        client.server().on_notification(
            "textDocument/publishDiagnostics",
            &serde_json::json!({
                "uri": uri, "version": 2,
                "diagnostics": [{"message": "stale", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );

        // The pull fails (-32603); the error re-consult finds only the stale
        // publish, which does not echo the current version, so it does not
        // settle the fresh debt.
        let outcome = pull_settling(&client, uri).await;
        assert!(
            matches!(outcome, PullSettlement::Unsettled),
            "a stale-version cached publish must not settle a fresh debt on the \
             pull error path (bug 85), got: {outcome:?}"
        );

        client.shutdown().await.expect("shutdown");
    }

    /// The pull error-path bug-99 re-consult still settles from a FRESH racing
    /// publish: one echoing the current version is evidence in hand.
    #[tokio::test]
    async fn pull_error_fresh_racing_publish_settles() {
        let (mut client, _dir) = spawn_pull_client(&["--pull-diagnostics", "--fail-pull"]).await;
        let uri = "file:///pull-honesty/fresh.pUll1";

        // The debt is at version 5, and a version-5 publish raced the failing
        // pull into the cache — a current-version echo.
        client.server().note_doc_version(uri, 5);
        client.server().on_notification(
            "textDocument/publishDiagnostics",
            &serde_json::json!({
                "uri": uri, "version": 5,
                "diagnostics": [{"message": "fresh", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );

        let diags = match pull_settling(&client, uri).await {
            PullSettlement::Settled(diags) => diags,
            PullSettlement::Unsettled => {
                Vec::new() // asserted non-empty below — Unsettled fails the test
            }
        };
        assert_eq!(
            diags.len(),
            1,
            "a fresh current-version racing publish must settle the debt (bug 99)"
        );
        assert_eq!(diags[0]["message"], "fresh");

        client.shutdown().await.expect("shutdown");
    }

    /// `ServerCancelled` is the spec's "busy, re-trigger": a single cancel is
    /// re-issued and the retry settles on the server's real report.
    #[tokio::test]
    async fn server_cancelled_retriggers_then_settles_on_success() {
        // The first pull answers -32802 (retriggerRequest: true); the bounded
        // re-trigger re-issues and the second pull serves the mock diagnostic.
        let (mut client, _dir) =
            spawn_pull_client(&["--pull-diagnostics", "--cancel-pull", "1"]).await;

        let diags = match pull_settling(&client, UNOPENED_URI).await {
            PullSettlement::Settled(diags) => diags,
            PullSettlement::Unsettled => Vec::new(),
        };
        assert!(
            diags
                .iter()
                .any(|d| d.get("source").and_then(Value::as_str) == Some("mockls")),
            "a single ServerCancelled must re-trigger and settle on the server's \
             real report, not stay unsettled: {diags:?}"
        );
        assert!(
            client.supports_pull_diagnostics(),
            "ServerCancelled must never downgrade the capability"
        );

        client.shutdown().await.expect("shutdown");
    }

    /// An exhausted re-trigger leaves the debt UNSETTLED (never a silent zero)
    /// and, being a transient fault, downgrades nothing.
    #[tokio::test]
    async fn exhausted_retrigger_leaves_debt_unsettled() {
        // 99 cancels far exceeds the re-trigger bound, so every attempt in the
        // loop answers -32802 and the pull never settles.
        let (mut client, _dir) =
            spawn_pull_client(&["--pull-diagnostics", "--cancel-pull", "99"]).await;

        let outcome = pull_settling(&client, UNOPENED_URI).await;
        assert!(
            matches!(outcome, PullSettlement::Unsettled),
            "a re-trigger the bound could not exhaust leaves the debt unsettled"
        );
        assert!(
            client.supports_pull_diagnostics(),
            "an exhausted ServerCancelled retry is transient — no downgrade"
        );

        client.shutdown().await.expect("shutdown");
    }

    /// `-32601` (`MethodNotFound`) is the sole downgrade evidence: the method is
    /// genuinely unsupported, so the capability is downgraded — but this round's
    /// debt still resolves unsettled, never a fabricated `Clean`.
    #[tokio::test]
    async fn method_not_found_records_downgrade() {
        // `--reject-pull` answers -32601 even though `--pull-diagnostics`
        // advertises the capability, so the retrieval path enters the pull arm.
        let (mut client, _dir) = spawn_pull_client(&["--pull-diagnostics", "--reject-pull"]).await;

        assert!(
            client.supports_pull_diagnostics(),
            "the capability is advertised before the rejecting pull"
        );

        let outcome = pull_settling(&client, UNOPENED_URI).await;
        assert!(
            matches!(outcome, PullSettlement::Unsettled),
            "even a -32601 leaves this round's debt unsettled, not Clean"
        );
        assert!(
            !client.supports_pull_diagnostics(),
            "-32601 evidence downgrades the pull capability (bug 84)"
        );

        client.shutdown().await.expect("shutdown");
    }

    /// The `retriggerRequest` hint drives the re-trigger decision: absent data
    /// and `true` retrigger; an explicit `false` declines.
    #[test]
    fn retrigger_requested_reads_the_cancellation_hint() {
        let with_data = |data: Option<Value>| -> anyhow::Error {
            LspResponseError {
                code: LSP_SERVER_CANCELLED,
                language: "x".to_string(),
                message: "cancelled".to_string(),
                data,
            }
            .into()
        };

        // No data: a bare ServerCancelled is "busy, ask again" → retrigger.
        assert!(retrigger_requested(&with_data(None)));
        // Explicit true → retrigger.
        assert!(retrigger_requested(&with_data(Some(
            serde_json::json!({ "retriggerRequest": true })
        ))));
        // Explicit false → decline.
        assert!(!retrigger_requested(&with_data(Some(
            serde_json::json!({ "retriggerRequest": false })
        ))));
        // Data present but flag absent → default to retrigger.
        assert!(retrigger_requested(&with_data(Some(
            serde_json::json!({ "other": 1 })
        ))));
    }
}
