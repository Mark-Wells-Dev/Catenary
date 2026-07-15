// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! LSP server representation: capabilities, shared state, and dispatch.
//!
//! `LspServer` is created at spawn time (before `initialize`) and is
//! the single source of truth for server behavior and state. Capabilities
//! are set once via [`LspServer::set_capabilities`] after the init
//! handshake. Notification dispatch (`on_notification`, `on_request`,
//! `on_shutdown`) updates diagnostics cache, progress, and server state.

use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Notify;
use tracing::{debug, info, trace};

use crate::bridge::filesystem_manager::ChangeKind;

use super::client::DiagnosticsCache;
use super::connection::Connection;
use super::extract;
use super::instance_key::{InstanceKey, Scope};
use super::protocol::RpcError;
use super::state::{ProgressTracker, ServerLifecycle};

/// LSP `WatchKind` bit: a new file matching the pattern was created.
const WATCH_KIND_CREATE: u8 = 1;
/// LSP `WatchKind` bit: a file matching the pattern was changed.
const WATCH_KIND_CHANGE: u8 = 2;
/// LSP `WatchKind` bit: a file matching the pattern was deleted.
const WATCH_KIND_DELETE: u8 = 4;
/// LSP `WatchKind` default when the registration omits `kind`: all three.
const WATCH_KIND_ALL: u8 = WATCH_KIND_CREATE | WATCH_KIND_CHANGE | WATCH_KIND_DELETE;

/// One registered `FileSystemWatcher` from a `didChangeWatchedFiles`
/// registration: a resolved glob plus the change kinds it cares about.
#[derive(Clone)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "pub(crate) consumed by WS31 ticket 03 (changed-set nudge)"
)]
pub(crate) struct ParsedWatcher {
    /// Compiled pattern (plain workspace-relative or base-relative), using LSP
    /// 3.17 glob semantics (`literal_separator(true)`, so `*` does not cross
    /// `/`). Shared with the rest of Catenary via [`crate::lsp::glob`].
    glob: crate::lsp::glob::GlobPattern,
    /// Bitmask of watched kinds: Create=1, Change=2, Delete=4 (LSP
    /// `WatchKind`); absent ⇒ all three (LSP default 7).
    kind: u8,
}

impl ParsedWatcher {
    /// Returns whether this watcher covers a changed path of the given
    /// semantic [`ChangeKind`].
    ///
    /// `rel` is the path relative to the workspace root; `abs` is the absolute
    /// path. The change is gated by both the watcher's kind mask
    /// ([`ChangeKind::Created`] needs the `Create` bit, [`ChangeKind::Changed`]
    /// needs the `Change` bit, [`ChangeKind::Deleted`] needs the `Delete` bit)
    /// and its glob. The glob/base matching is delegated to
    /// [`crate::lsp::glob::GlobPattern::matches_paths`] (LSP 3.17 semantics): a
    /// base-relative pattern matches the path relative to its base; a
    /// workspace-relative pattern matches the root-relative path with the
    /// absolute path as a fallback (servers register both forms). For a
    /// deletion the file is already gone from disk, but the stored `rel`/`abs`
    /// paths still match the registered glob.
    pub(crate) fn covers(
        &self,
        rel: &std::path::Path,
        abs: &std::path::Path,
        kind: ChangeKind,
    ) -> bool {
        let required = match kind {
            ChangeKind::Created => WATCH_KIND_CREATE,
            ChangeKind::Changed => WATCH_KIND_CHANGE,
            ChangeKind::Deleted => WATCH_KIND_DELETE,
        };
        if self.kind & required == 0 {
            return false;
        }
        self.glob.matches_paths(rel, abs)
    }

    /// Parses a single `FileSystemWatcher` JSON value into a [`ParsedWatcher`].
    ///
    /// The `globPattern` is either a string (workspace-relative or absolute)
    /// or a relative-pattern object `{ baseUri, pattern }` (Catenary
    /// advertises `relativePatternSupport: true`). The optional `kind` field
    /// is an LSP `WatchKind` bitmask; absent ⇒ [`WATCH_KIND_ALL`].
    ///
    /// Returns `None` for a malformed entry (missing `globPattern`, or an
    /// object whose `pattern` is missing or won't compile), which the caller
    /// skips with a `debug!` rather than failing the whole registration. The
    /// `globPattern` (string or `{ baseUri, pattern }`) is parsed via
    /// [`crate::lsp::glob::GlobPattern`], which compiles with LSP 3.17 semantics
    /// and percent-decodes the `baseUri`.
    ///
    /// An object-form `globPattern` whose `baseUri` is missing or non-`file://`
    /// degrades gracefully: the `baseUri` is dropped and the `pattern` is
    /// matched workspace-relative (the pre-relative-pattern behavior) rather
    /// than discarding the whole watcher. A `pattern` that won't *compile* still
    /// drops — degradation never builds a broken matcher.
    fn from_value(watcher: &Value) -> Option<Self> {
        let glob_value = watcher.get("globPattern")?;
        let glob = match crate::lsp::glob::GlobPattern::from_value(glob_value) {
            Ok(glob) => glob,
            // `from_value` failed. If the value is an object with a `pattern`
            // string, the failure was the `baseUri` (missing / non-`file://`) —
            // degrade to a workspace-relative Plain glob on the `pattern`. A
            // pattern that won't compile makes `plain` fail too, so the watcher
            // still drops.
            Err(_) => glob_value
                .as_object()
                .and_then(|obj| obj.get("pattern"))
                .and_then(Value::as_str)
                .and_then(|pattern| crate::lsp::glob::GlobPattern::plain(pattern).ok())?,
        };

        let kind = watcher
            .get("kind")
            .and_then(Value::as_u64)
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(WATCH_KIND_ALL);

        Some(Self { glob, kind })
    }
}

/// Complete representation of a remote LSP server.
///
/// Created at spawn time with empty `OnceLock` fields. Capabilities are
/// populated once via [`Self::set_capabilities`] after the `initialize`
/// handshake completes. Shared via `Arc<LspServer>` between
/// [`super::LspClient`] and [`super::connection::Connection`]. All
/// runtime fields use interior mutability so readers never need a lock.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability flags from LSP init"
)]
pub struct LspServer {
    // ── Capabilities (set once via set_capabilities) ──────────────
    /// Raw server capabilities from the `initialize` response.
    capabilities: OnceLock<Value>,

    supports_pull_diagnostics: AtomicBool,
    /// Engine-internal casing: when set, this server is never advertised the
    /// `textDocument.diagnostic` client capability and is never sent
    /// `textDocument/diagnostic` — advertised pull *or* best-effort probe — even
    /// if it spontaneously advertises `diagnosticProvider` (misc 157). Set once
    /// at construction from
    /// [`super::server_behavior::ServerProfile::suppresses_pull_diagnostics`],
    /// immutable thereafter.
    pull_suppressed: bool,
    /// Engine-internal casing: the server contractually publishes diagnostics
    /// for every opened document (misc 187). Arms the retrieval evidence bar by
    /// declaration, before this connection has demonstrated a publish
    /// ([`Self::has_ever_published`] resets on every respawn). Set once at
    /// construction from
    /// [`super::server_behavior::ServerProfile::declares_push`], immutable
    /// thereafter.
    declares_push: bool,
    /// Engine-internal casing: the declared debounce window for a
    /// [`crate::recipes::Discipline::Debounce`] server (diagnostics-debt 05).
    /// `Some(ms)` only for a debounce-discipline manifest row carrying the
    /// declared `debounce_ms` constant — the retrieval evidence bar awaits the
    /// version echo bounded by this constant (data riding the pin, never a
    /// measured guess), rather than by the generic dead-air budget. Set once at
    /// construction from
    /// [`super::server_behavior::ServerProfile::debounce_ms`], immutable
    /// thereafter.
    debounce_ms: Option<u64>,
    /// Engine-internal casing: the server's **verified discipline owes an
    /// answer** for a round that stimulated it (diagnostics-debt 05) — a
    /// declared-push server (misc 187) or a debounce-discipline server. When the
    /// retrieval evidence bar arms and expires for such a server, the discipline
    /// said an answer was owed and none came, so the file resolves the
    /// fault floor's verified-contract-violation wording (DESIGN §"The floor is
    /// fault attribution") and the round strikes the ledger — never a false
    /// `[clean]`. A merely demonstrated-push server owes no contract, so its
    /// expiry stays the softer silent wording. Set once at construction from
    /// [`super::server_behavior::ServerProfile::owes_answer`], immutable
    /// thereafter.
    owes_answer: bool,
    /// Engine-internal casing: the server is a **scan-discipline** server
    /// (marksman-class; misc 196). A scan server owes its whole-workspace answer,
    /// so a `workspace/diagnostic` pull that goes unanswered/refused while the
    /// server is alive is a verified-contract violation (the floor's scan arm).
    /// Round-conditional — unlike [`Self::owes_answer`] it is read at the
    /// workspace-pull seam together with the pull's outcome. Set once at
    /// construction from [`super::server_behavior::ServerProfile::is_scan`],
    /// immutable thereafter.
    is_scan: bool,
    /// Engine-internal casing: the server is a **diff-discipline** server
    /// (marksman diff-only; misc 196). A diff server owes a publish on any round
    /// that delivered its save trigger, so an alive diff server silent after a
    /// delivered `didSave` is a verified-contract violation (the floor's diff arm);
    /// a round with no delivered trigger owes nothing. Round-conditional — read at
    /// the per-file batch seam together with the "a save was delivered this round"
    /// signal. Set once at construction from
    /// [`super::server_behavior::ServerProfile::is_diff`], immutable thereafter.
    is_diff: bool,
    /// Blessed/unverified classification: when set, this server is an unverified
    /// custom def and is **enrichment-only** (diagnostics-debt 04b / DESIGN
    /// §"The blessed set") — never a diagnostics source. [`Self::supports_diagnostics`]
    /// returns `false` for it regardless of advertised capabilities, so the
    /// `diagnostic_servers` gate excludes it, the held-open batch sync lifecycle
    /// never engages it, and any publish it sends anyway is never collected. Its
    /// query capabilities (definition, references, symbols) and watched-files
    /// delivery are untouched: only diagnostics listening is withheld. Set once at
    /// construction from
    /// [`super::server_behavior::ServerProfile::is_enrichment_only`], immutable
    /// thereafter.
    enrichment_only: bool,
    supports_workspace_diagnostics: OnceLock<bool>,
    supports_text_document_sync: OnceLock<bool>,
    supports_definition: OnceLock<bool>,
    supports_references: OnceLock<bool>,
    supports_document_symbols: OnceLock<bool>,
    supports_workspace_symbols: OnceLock<bool>,
    supports_workspace_symbol_resolve: OnceLock<bool>,
    supports_rename: OnceLock<bool>,
    supports_type_definition: OnceLock<bool>,
    supports_implementation: OnceLock<bool>,
    supports_call_hierarchy: OnceLock<bool>,
    supports_type_hierarchy: OnceLock<bool>,
    supports_code_action: OnceLock<bool>,

    // ── Diagnostics ───────────────────────────────────────────────
    pub(crate) diagnostics: DiagnosticsCache,
    pub(crate) diagnostics_generation: Arc<Mutex<HashMap<String, u64>>>,
    pub(crate) diagnostics_notify: Arc<Notify>,
    /// Latest document version this side has sent per URI
    /// (`didOpen`/`didChange`), monotonic across close/reopen — the
    /// reference point for the publish staleness gate (bug 101, heard-stale
    /// leg): a version-carrying `publishDiagnostics` computed against an
    /// older version than the one last sent is a straggler from a previous
    /// round, and caching it would overwrite fresher evidence.
    doc_versions: Mutex<HashMap<String, i32>>,

    // ── Capability discovery ──────────────────────────────────────
    pub(crate) capability_notify: Arc<Notify>,

    // ── Progress ──────────────────────────────────────────────────
    pub(crate) progress: Arc<Mutex<ProgressTracker>>,
    pub(crate) progress_notify: Arc<Notify>,

    // ── Lifecycle ─────────────────────────────────────────────────
    /// Unified server lifecycle state. See [`ServerLifecycle`].
    pub(crate) lifecycle: Arc<Mutex<ServerLifecycle>>,
    /// Wakes waiters on lifecycle transitions.
    pub(crate) state_notify: Arc<Notify>,
    /// Set on first `Busy` transition (runtime capability discovery).
    pub(crate) ever_busy: AtomicBool,

    // ── Observation flags ─────────────────────────────────────────
    pub(crate) publishes_version: Arc<AtomicBool>,
    /// Set on the first `textDocument/publishDiagnostics` heard on this
    /// connection — runtime evidence the server is a *push*-diagnostics
    /// server. Arms the retrieval evidence bar: once a server has
    /// demonstrably pushed, a never-heard file may not render `[clean]`
    /// until the pipeline has evidence the server reacted to the batch's
    /// `didOpen`/`didSave` (bug 99 residual / bug 101 / misc 156). Resets
    /// with the connection (a fresh spawn is a fresh `LspServer`).
    ever_published: AtomicBool,
    /// Set on the first *successful* best-effort `textDocument/diagnostic`
    /// probe answer. An answered probe is a working on-demand evidence
    /// channel (the bug-74 lattice shape: no advertised `diagnosticProvider`,
    /// but the request is served) — retrieval can always ask directly, so
    /// the publish-evidence wait is unnecessary for such a server.
    probe_answered: AtomicBool,

    // ── Identity ──────────────────────────────────────────────────
    /// Language identifier (known at spawn time, immutable).
    language_id: String,
    /// Server config name (known at spawn time, immutable).
    server_name: String,
    /// Routing scope (set once after `initialize`, via `OnceLock`).
    scope: OnceLock<Scope>,

    // ── Configuration ─────────────────────────────────────────────
    settings: Option<Value>,

    // ── Process tree ──────────────────────────────────────────
    /// Tree monitor for idle detection. Created when the connection is set.
    /// Sole owner is the idle detection loop; all access via [`Self::sample_tree`].
    tree_monitor: Mutex<Option<catenary_proc::TreeMonitor>>,

    // ── Dynamic registrations ────────────────────────────────
    /// Registration IDs for `workspace/didChangeConfiguration`.
    /// Tracked per-ID so selective unregistration works correctly
    /// when a server holds multiple registrations.
    config_change_registrations: Mutex<HashSet<String>>,

    /// `workspace/didChangeWatchedFiles` registrations, keyed by registration
    /// id. The conditional nudge (WS31 Consumer A) fires from these.
    watched_files_registrations: Mutex<HashMap<String, Vec<ParsedWatcher>>>,

    // ── Transport ───────────────────────────────────────────────
    connection: OnceLock<Connection>,

    // ── State snapshot ───────────────────────────────────────
    /// Daemon-owned `state.json` writer. Lifecycle, progress, and message
    /// transitions mutate the server board and mark the snapshot dirty. Set
    /// after init via [`Self::set_snapshot`]. `None` in doctor/test contexts.
    snapshot: OnceLock<Arc<crate::state_snapshot::SnapshotWriter>>,
}

impl LspServer {
    /// Creates a new `LspServer` with default state.
    ///
    /// `language_id` and `server_name` are known at spawn time and never
    /// change. The routing scope is set via [`Self::set_scope`] before
    /// `initialize` so protocol messages carry `scope_root` from the start.
    ///
    /// Call [`Self::set_capabilities`] after the `initialize` handshake
    /// to populate capability fields.
    #[must_use]
    pub fn new(language_id: String, server_name: String, settings: Option<Value>) -> Self {
        let profile = super::server_behavior::ServerProfile::for_server(server_name.as_str());
        let pull_suppressed = profile.suppresses_pull_diagnostics();
        let declares_push = profile.declares_push();
        let debounce_ms = profile.debounce_ms();
        let owes_answer = profile.owes_answer();
        let is_scan = profile.is_scan();
        let is_diff = profile.is_diff();
        let enrichment_only = profile.is_enrichment_only();
        Self {
            capabilities: OnceLock::new(),
            supports_pull_diagnostics: AtomicBool::new(false),
            pull_suppressed,
            declares_push,
            debounce_ms,
            owes_answer,
            is_scan,
            is_diff,
            enrichment_only,
            supports_workspace_diagnostics: OnceLock::new(),
            supports_text_document_sync: OnceLock::new(),
            supports_definition: OnceLock::new(),
            supports_references: OnceLock::new(),
            supports_document_symbols: OnceLock::new(),
            supports_workspace_symbols: OnceLock::new(),
            supports_workspace_symbol_resolve: OnceLock::new(),
            supports_rename: OnceLock::new(),
            supports_type_definition: OnceLock::new(),
            supports_implementation: OnceLock::new(),
            supports_call_hierarchy: OnceLock::new(),
            supports_type_hierarchy: OnceLock::new(),
            supports_code_action: OnceLock::new(),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_generation: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_notify: Arc::new(Notify::new()),
            doc_versions: Mutex::new(HashMap::new()),
            capability_notify: Arc::new(Notify::new()),
            progress: Arc::new(Mutex::new(ProgressTracker::new())),
            progress_notify: Arc::new(Notify::new()),
            lifecycle: Arc::new(Mutex::new(ServerLifecycle::Initializing)),
            state_notify: Arc::new(Notify::new()),
            ever_busy: AtomicBool::new(false),
            publishes_version: Arc::new(AtomicBool::new(false)),
            ever_published: AtomicBool::new(false),
            probe_answered: AtomicBool::new(false),
            language_id,
            server_name,
            scope: OnceLock::new(),
            settings,
            config_change_registrations: Mutex::new(HashSet::new()),
            watched_files_registrations: Mutex::new(HashMap::new()),
            tree_monitor: Mutex::new(None),
            connection: OnceLock::new(),
            snapshot: OnceLock::new(),
        }
    }

    /// Returns the server settings, if configured.
    pub(crate) const fn settings(&self) -> Option<&Value> {
        self.settings.as_ref()
    }

    /// Resolves a `workspace/configuration` item.
    ///
    /// Each per-root instance has pre-merged flat settings — no
    /// per-root overlay resolution needed. Resolves the requested
    /// section against the instance's settings.
    fn resolve_configuration(&self, section: Option<&str>, _scope_uri: Option<&str>) -> Value {
        resolve_section(self.settings.as_ref(), section)
    }

    // ── Identity accessors ──────────────────────────────────────────

    /// Returns the language identifier.
    #[must_use]
    pub fn language_id(&self) -> &str {
        &self.language_id
    }

    /// Returns the server config name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Returns the routing scope (`None` before [`Self::set_scope`]).
    #[must_use]
    pub fn scope(&self) -> Option<&Scope> {
        self.scope.get()
    }

    /// Sets the routing scope. Called once before `initialize` so the
    /// reader loop has it for all protocol messages.
    ///
    /// Subsequent calls are no-ops (the `OnceLock` ignores them).
    pub fn set_scope(&self, scope: Scope) {
        let _ = self.scope.set(scope);
    }

    /// Constructs the full [`InstanceKey`] from stored components.
    ///
    /// Returns `None` if scope hasn't been set yet (pre-init).
    /// After init, always returns `Some`.
    #[must_use]
    pub fn key(&self) -> Option<InstanceKey> {
        self.scope.get().map(|s| {
            InstanceKey::new(
                self.language_id.clone(),
                self.server_name.clone(),
                s.clone(),
            )
        })
    }

    /// Sets capabilities from the `initialize` response. Called once.
    ///
    /// Extracts all capability flags and stores the raw capabilities.
    /// Subsequent calls are no-ops (the `OnceLock` ignores them).
    pub fn set_capabilities(&self, capabilities: Value) {
        // LSP capabilities are `boolean | Options`. `true` or an options
        // object means supported; `false`, `null`, or absent means not.
        let has = |key: &str| {
            capabilities
                .get(key)
                .is_some_and(|v| v.as_bool() != Some(false) && !v.is_null())
        };
        // A pull-suppressed server (misc 157) never gets pull turned on, even if
        // it spontaneously advertises `diagnosticProvider` — the client-side pull
        // path stays structurally unreachable for it.
        self.supports_pull_diagnostics.store(
            !self.pull_suppressed && has("diagnosticProvider"),
            Ordering::SeqCst,
        );
        // `workspace/diagnostic` is gated on the nested
        // `diagnosticProvider.workspaceDiagnostics` boolean (LSP 3.17), not on
        // the provider's mere presence — a server can pull per-document without
        // serving the whole-workspace request.
        let _ = self.supports_workspace_diagnostics.set(
            capabilities
                .get("diagnosticProvider")
                .and_then(|d| d.get("workspaceDiagnostics"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        let _ = self
            .supports_text_document_sync
            .set(has("textDocumentSync"));
        let _ = self.supports_definition.set(has("definitionProvider"));
        let _ = self.supports_references.set(has("referencesProvider"));
        let _ = self
            .supports_document_symbols
            .set(has("documentSymbolProvider"));
        let _ = self
            .supports_workspace_symbols
            .set(has("workspaceSymbolProvider"));
        let _ = self.supports_workspace_symbol_resolve.set(
            capabilities
                .get("workspaceSymbolProvider")
                .and_then(|v| v.get("resolveProvider"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        let _ = self.supports_rename.set(has("renameProvider"));
        let _ = self
            .supports_type_definition
            .set(has("typeDefinitionProvider"));
        let _ = self
            .supports_implementation
            .set(has("implementationProvider"));
        let _ = self
            .supports_call_hierarchy
            .set(has("callHierarchyProvider"));
        let _ = self
            .supports_type_hierarchy
            .set(has("typeHierarchyProvider"));
        let _ = self.supports_code_action.set(has("codeActionProvider"));
        let _ = self.capabilities.set(capabilities);
    }

    /// Returns the raw server capabilities.
    ///
    /// Returns an empty object before [`Self::set_capabilities`] is called.
    pub fn capabilities(&self) -> &Value {
        static EMPTY: OnceLock<Value> = OnceLock::new();
        self.capabilities
            .get()
            .unwrap_or_else(|| EMPTY.get_or_init(|| Value::Object(serde_json::Map::new())))
    }

    /// Returns whether the server can produce diagnostics.
    ///
    /// True if the server advertises `textDocumentSync` (push diagnostics
    /// via `publishDiagnostics`) or `diagnosticProvider` (pull diagnostics
    /// via `textDocument/diagnostic`). Used as the capability gate for
    /// diagnostic dispatch in [`super::LspClientManager::get_servers`].
    ///
    /// **Always `false` for an enrichment-only (unverified) server**
    /// (diagnostics-debt 04b / DESIGN §"The blessed set"), regardless of what it
    /// advertised: an unverified server is never a diagnostics source, so the
    /// `diagnostic_servers` gate excludes it, the held-open batch sync lifecycle
    /// never engages it, and a stray publish it sends anyway is never collected.
    /// Its query capabilities and watched-files delivery are unaffected — those
    /// gate on other predicates.
    pub fn supports_diagnostics(&self) -> bool {
        if self.enrichment_only {
            return false;
        }
        self.supports_text_document_sync
            .get()
            .copied()
            .unwrap_or(false)
            || self.supports_pull_diagnostics()
    }

    /// Returns whether this server is **enrichment-only** — unverified, so never a
    /// diagnostics source (diagnostics-debt 04b).
    ///
    /// Set once at construction from
    /// [`super::server_behavior::ServerProfile::is_enrichment_only`] (a custom
    /// `[lsp.server.*]` def absent from the blessed manifest). Immutable
    /// thereafter. See [`Self::supports_diagnostics`] for the behavioural
    /// consequence.
    pub const fn is_enrichment_only(&self) -> bool {
        self.enrichment_only
    }

    /// Returns whether the server supports pull diagnostics.
    ///
    /// Initially set from the `diagnosticProvider` capability. Can be
    /// downgraded to `false` at runtime via [`Self::downgrade_pull_diagnostics`]
    /// if the server fails the actual request. Always `false` for a
    /// pull-suppressed server (misc 157), regardless of what it advertised.
    pub fn supports_pull_diagnostics(&self) -> bool {
        !self.pull_suppressed && self.supports_pull_diagnostics.load(Ordering::SeqCst)
    }

    /// Returns whether this server is cased to never receive `textDocument/diagnostic`.
    ///
    /// Engine-internal per-server casing (misc 157): a suppressed server is never
    /// sent an advertised pull *or* the best-effort probe, so its native pushes
    /// are the sole diagnostic channel. Set once at construction; see
    /// [`super::server_behavior::ServerProfile::suppresses_pull_diagnostics`].
    pub const fn pull_suppressed(&self) -> bool {
        self.pull_suppressed
    }

    /// Returns whether this server is cased as a contractual push publisher.
    ///
    /// Engine-internal per-server casing (misc 187): a declared-push server
    /// publishes diagnostics for every opened document — a publish on every
    /// `didOpen`, an explicit `[]` for clean — so the retrieval evidence bar
    /// arms on the declaration alone, before this connection has heard a single
    /// publish. Set once at construction; see
    /// [`super::server_behavior::ServerProfile::declares_push`].
    pub(crate) const fn declares_push(&self) -> bool {
        self.declares_push
    }

    /// The declared debounce window for a debounce-discipline server, or `None`
    /// (diagnostics-debt 05).
    ///
    /// `Some(ms)` only for a [`crate::recipes::Discipline::Debounce`] manifest row
    /// carrying the declared `debounce_ms` constant. The retrieval evidence bar
    /// awaits the version echo bounded by this constant (converted to a
    /// sample budget) rather than by the generic dead-air budget — an
    /// arrival-based gate on the declared bound, never silence-interpretation. Set
    /// once at construction; see
    /// [`super::server_behavior::ServerProfile::debounce_ms`].
    pub(crate) const fn debounce_ms(&self) -> Option<u64> {
        self.debounce_ms
    }

    /// Whether this server's **verified discipline owes an answer** for a round
    /// that stimulated it (diagnostics-debt 05).
    ///
    /// True for a declared-push server (misc 187) or a debounce-discipline
    /// server. When the retrieval evidence bar arms and expires for such a
    /// server, the discipline said an answer was owed this round and none came —
    /// the fault floor's verified-contract-violation arm (DESIGN §"The floor is
    /// fault attribution"): the file resolves that wording and the round strikes
    /// the ledger. A merely demonstrated-push server owes no verified contract,
    /// so its expiry stays the softer silent wording. Set once at construction;
    /// see [`super::server_behavior::ServerProfile::owes_answer`].
    pub(crate) const fn owes_answer(&self) -> bool {
        self.owes_answer
    }

    /// Whether this server is a **scan-discipline** server (marksman-class; misc
    /// 196).
    ///
    /// A scan server owes its whole-workspace answer, so a `workspace/diagnostic`
    /// pull that goes unanswered/refused while the server is alive is a
    /// verified-contract violation (the floor's scan arm). Round-conditional, read
    /// at the workspace-pull seam together with the pull's outcome — unlike the
    /// static [`Self::owes_answer`]. Set once at construction; see
    /// [`super::server_behavior::ServerProfile::is_scan`].
    pub(crate) const fn is_scan(&self) -> bool {
        self.is_scan
    }

    /// Whether this server is a **diff-discipline** server (marksman diff-only;
    /// misc 196).
    ///
    /// A diff server owes a publish on any round that delivered its save trigger,
    /// so an alive diff server silent after a delivered `didSave` is a
    /// verified-contract violation (the floor's diff arm); a round with no
    /// delivered trigger owes nothing. Round-conditional, read at the per-file
    /// batch seam together with the "a save was delivered this round" signal. Set
    /// once at construction; see
    /// [`super::server_behavior::ServerProfile::is_diff`].
    pub(crate) const fn is_diff(&self) -> bool {
        self.is_diff
    }

    /// Returns whether any `textDocument/publishDiagnostics` has been heard on
    /// this connection — runtime evidence the server is a push server.
    ///
    /// Arms the retrieval evidence bar (bug 99 residual / misc 156): a server
    /// that has demonstrably pushed will react to a `didOpen`/`didSave` with a
    /// publish (possibly empty — heard-empty clean), so a never-heard file is
    /// not rendered `[clean]` from mere absence while that reaction may still
    /// be pending in a silent debounce.
    pub(crate) fn has_ever_published(&self) -> bool {
        self.ever_published.load(Ordering::SeqCst)
    }

    /// Returns whether a best-effort `textDocument/diagnostic` probe has ever
    /// been *answered* (not rejected) on this connection.
    ///
    /// An answered probe is a working on-demand evidence channel — the bug-74
    /// lattice shape — so retrieval can always ask the server directly and the
    /// publish-evidence wait is unnecessary.
    pub(crate) fn has_answered_probe(&self) -> bool {
        self.probe_answered.load(Ordering::SeqCst)
    }

    /// Records that a best-effort `textDocument/diagnostic` probe was answered.
    ///
    /// See [`Self::has_answered_probe`].
    pub(crate) fn note_probe_answered(&self) {
        self.probe_answered.store(true, Ordering::SeqCst);
    }

    /// Returns whether the server supports whole-workspace pull diagnostics.
    ///
    /// Set from the nested `diagnosticProvider.workspaceDiagnostics` capability
    /// (LSP 3.17). Gates the whole-root `catenary diagnostics .` scope onto a
    /// single `workspace/diagnostic` request off the server's existing project
    /// model, in place of the per-file fan-out (workstream 37 ticket 04).
    ///
    /// **Always `false` for an enrichment-only server** (diagnostics-debt 04b): an
    /// unverified server is never a diagnostics source, even for a whole-root
    /// scope, regardless of what its `initialize` response advertised — the same
    /// stance [`Self::supports_diagnostics`] takes for the per-file path.
    pub fn supports_workspace_diagnostics(&self) -> bool {
        !self.enrichment_only
            && self
                .supports_workspace_diagnostics
                .get()
                .copied()
                .unwrap_or(false)
    }

    /// Returns the diagnostic pull `identifier`, if the server advertised one.
    ///
    /// The optional `diagnosticProvider.identifier` disambiguates pull requests
    /// when a server exposes several diagnostic sources; it rides the
    /// `workspace/diagnostic` params so a repeated pull can be attributed and,
    /// with result IDs, served incrementally.
    pub fn diagnostic_identifier(&self) -> Option<String> {
        self.capabilities()
            .get("diagnosticProvider")
            .and_then(|d| d.get("identifier"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Permanently disables pull diagnostics for this server.
    ///
    /// Called only on `-32601` (`MethodNotFound`) evidence — the method is
    /// genuinely unsupported (bug 84). A transient pull failure (busy,
    /// `InternalError`, transport fault) must NOT downgrade: the next round
    /// retries the pull. Subsequent calls to
    /// [`Self::supports_pull_diagnostics`] return `false`.
    pub fn downgrade_pull_diagnostics(&self) {
        self.supports_pull_diagnostics
            .store(false, Ordering::SeqCst);
        info!("pull diagnostics downgraded to push-only");
    }

    /// Returns whether the server advertises `definitionProvider`.
    pub fn supports_definition(&self) -> bool {
        self.supports_definition.get().copied().unwrap_or(false)
    }

    /// Returns whether the server advertises `referencesProvider`.
    pub fn supports_references(&self) -> bool {
        self.supports_references.get().copied().unwrap_or(false)
    }

    /// Returns whether the server advertises `documentSymbolProvider`.
    pub fn supports_document_symbols(&self) -> bool {
        self.supports_document_symbols
            .get()
            .copied()
            .unwrap_or(false)
    }

    /// Returns whether the server advertises `workspaceSymbolProvider`.
    pub fn supports_workspace_symbols(&self) -> bool {
        self.supports_workspace_symbols
            .get()
            .copied()
            .unwrap_or(false)
    }

    /// Returns whether the server advertises `workspaceSymbolProvider.resolveProvider`.
    pub fn supports_workspace_symbol_resolve(&self) -> bool {
        self.supports_workspace_symbol_resolve
            .get()
            .copied()
            .unwrap_or(false)
    }

    /// Returns whether the server advertises `renameProvider`.
    pub fn supports_rename(&self) -> bool {
        self.supports_rename.get().copied().unwrap_or(false)
    }

    /// Returns whether the server advertises `typeDefinitionProvider`.
    pub fn supports_type_definition(&self) -> bool {
        self.supports_type_definition
            .get()
            .copied()
            .unwrap_or(false)
    }

    /// Returns whether the server advertises `implementationProvider`.
    pub fn supports_implementation(&self) -> bool {
        self.supports_implementation.get().copied().unwrap_or(false)
    }

    /// Returns whether the server advertises `callHierarchyProvider`.
    pub fn supports_call_hierarchy(&self) -> bool {
        self.supports_call_hierarchy.get().copied().unwrap_or(false)
    }

    /// Returns whether the server advertises `typeHierarchyProvider`.
    pub fn supports_type_hierarchy(&self) -> bool {
        self.supports_type_hierarchy.get().copied().unwrap_or(false)
    }

    /// Returns whether the server advertises `codeActionProvider`.
    pub fn supports_code_action(&self) -> bool {
        self.supports_code_action.get().copied().unwrap_or(false)
    }

    /// Returns whether the server has ever been in `Busy` state
    /// (i.e., has ever sent `$/progress` begin).
    pub fn sends_progress(&self) -> bool {
        self.ever_busy.load(Ordering::SeqCst)
    }

    /// Returns the number of in-flight progress tokens.
    ///
    /// Derived from the lifecycle enum: `Busy(n)` → `n`, all others → `0`.
    pub fn in_progress_count(&self) -> u32 {
        match self.lifecycle() {
            ServerLifecycle::Busy(n) => n,
            _ => 0,
        }
    }

    /// Returns the current lifecycle state.
    pub fn lifecycle(&self) -> ServerLifecycle {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sets the lifecycle state and wakes waiters.
    ///
    /// Emits a `debug!()` event on every transition so the firehose and the
    /// daemon snapshot track server state. On first transition to a terminal
    /// state (`Dead` or `Failed`), additionally emits a `warn!()` — surfaced as
    /// a health finding on the TUI and recorded in the firehose (the
    /// user-notification queue retired in tui-rework 04).
    pub(crate) fn set_lifecycle(&self, state: ServerLifecycle) {
        let is_terminal = state.is_terminal();
        // Keep a copy for persistence; `state` itself moves into the lock.
        let persist = state.clone();
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_terminal = lifecycle.is_terminal();
        *lifecycle = state;
        drop(lifecycle);

        self.persist_state(&persist);

        // Emit user-facing notification on first transition to terminal state.
        if !was_terminal
            && is_terminal
            && let Some(key) = self.key()
        {
            tracing::warn!(
                source = crate::source::Source::LspLifecycle.as_str(),
                language = key.language_id.as_str(),
                server = key.server.as_str(),
                "Language server unavailable: {} ({}) \u{2014} \
                 diagnostics unavailable for {} files. \
                 grep and glob still work but without \
                 language server enrichment.",
                key.language_id,
                key.server,
                key.language_id,
            );
        }

        self.state_notify.notify_waiters();
    }

    /// Derives the lifecycle from the single work-done-token registry counts
    /// and applies it (misc 200).
    ///
    /// `begun` is the count of tokens with an open `$/progress` `begin`
    /// bracket; `created` is the count of tokens announced via
    /// `window/workDoneProgress/create` but not yet begun. The derivation:
    /// `begun > 0` → [`ServerLifecycle::Busy`] (unchanged busy semantics),
    /// else `created > 0` → [`ServerLifecycle::Pending`] (announced, not yet
    /// started — the pending half both settle seams hold on), else
    /// [`ServerLifecycle::Healthy`] (all tokens retired).
    ///
    /// A no-op when the server is already terminal (a dead server produces no
    /// more progress) and when the derived state equals the current one — so a
    /// `report` on an already-`Busy` server churns no snapshot write. Persists
    /// and wakes state waiters on a real transition.
    pub(crate) fn apply_progress_lifecycle(&self, begun: u32, created: u32) {
        let derived = if begun > 0 {
            ServerLifecycle::Busy(begun)
        } else if created > 0 {
            ServerLifecycle::Pending(created)
        } else {
            ServerLifecycle::Healthy
        };

        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.is_terminal() || *lifecycle == derived {
            return;
        }
        *lifecycle = derived.clone();
        drop(lifecycle);

        self.persist_state(&derived);
        self.state_notify.notify_waiters();
    }

    // ── State snapshot ───────────────────────────────────────────

    /// Sets the `state.json` snapshot writer for live-state mirroring.
    ///
    /// Called once after init by [`super::manager::LspClientManager`].
    /// Subsequent calls are no-ops (the `OnceLock` ignores them).
    pub fn set_snapshot(&self, writer: Arc<crate::state_snapshot::SnapshotWriter>) {
        let _ = self.snapshot.set(writer);
    }

    /// Mirrors the lifecycle state to the `state.json` snapshot.
    ///
    /// The snapshot carries the full [`ServerLifecycle`] variant. No-op when
    /// the snapshot writer or the instance key is unavailable.
    fn persist_state(&self, state: &ServerLifecycle) {
        if let (Some(writer), Some(key)) = (self.snapshot.get(), self.key()) {
            writer.update_state(&key, state);
        }
    }

    /// Mirrors progress state to the `state.json` snapshot.
    ///
    /// The snapshot carries title, current message, and percentage. No-op when
    /// the snapshot writer or the instance key is unavailable.
    fn persist_progress(&self, title: Option<&str>, message: Option<&str>, pct: Option<u32>) {
        if let (Some(writer), Some(key)) = (self.snapshot.get(), self.key()) {
            writer.update_progress(&key, title, message, pct);
        }
    }

    /// Mirrors a server message to the `state.json` snapshot.
    ///
    /// `level` is the LSP `MessageType` mapped to a severity tag. No-op when
    /// the snapshot writer or the instance key is unavailable.
    fn persist_message(&self, level: &str, message: &str) {
        if let (Some(writer), Some(key)) = (self.snapshot.get(), self.key()) {
            writer.update_message(&key, level, message);
        }
    }

    // ── Transport ────────────────────────────────────────────────

    /// Sets the connection after two-phase construction.
    ///
    /// Called once after `Connection::new()` with the `Arc<LspServer>`
    /// already wrapped. Also creates the [`catenary_proc::TreeMonitor`]
    /// for the server's process tree. Subsequent calls are no-ops.
    pub fn set_connection(&self, connection: Connection) {
        let pid = connection.pid();
        let _ = self.connection.set(connection);
        if let Some(pid) = pid
            && let Some(tm) = catenary_proc::TreeMonitor::new(pid)
        {
            *self
                .tree_monitor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tm);
        }
    }

    /// Returns a reference to the connection, if set.
    fn connection(&self) -> Option<&Connection> {
        self.connection.get()
    }

    /// Sends a request and waits for the response.
    ///
    /// Delegates to [`Connection::request`] for transport and failure
    /// detection. Returns an error if the connection has not been set.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection is not established or the
    /// request fails.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        parent_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Value> {
        self.connection()
            .ok_or_else(|| anyhow::anyhow!("connection not established"))?
            .request(method, params, parent_id, cancel)
            .await
    }

    /// Sends a notification (no response expected).
    ///
    /// # Errors
    ///
    /// Returns an error if the connection is not established or the
    /// notification fails.
    pub async fn notify(&self, method: &str, params: Value, parent_id: Option<&str>) -> Result<()> {
        self.connection()
            .ok_or_else(|| anyhow::anyhow!("connection not established"))?
            .notify(method, params, parent_id)
            .await
    }

    /// Returns whether the server process is alive.
    pub fn is_alive(&self) -> bool {
        self.connection().is_some_and(Connection::is_alive)
    }

    /// Returns the PID of the server process.
    pub fn pid(&self) -> Option<u32> {
        self.connection().and_then(Connection::pid)
    }

    /// Samples the process monitor for CPU-tick failure detection.
    pub fn sample_monitor(&self) -> Option<catenary_proc::ProcessDelta> {
        self.connection()?.sample_monitor()
    }

    /// Returns a shared reference to the alive flag.
    pub fn alive_flag(&self) -> Option<Arc<AtomicBool>> {
        self.connection().map(Connection::alive_flag)
    }

    /// Drains the stdout pipe so all buffered server messages have
    /// been processed by the reader loop.
    ///
    /// See [`Connection::drain`] for the mechanism.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection is not established or the
    /// drain fails.
    pub async fn drain(&self) -> anyhow::Result<()> {
        self.connection()
            .ok_or_else(|| anyhow::anyhow!("connection not established"))?
            .drain()
            .await
    }

    // ── Process tree ─────────────────────────────────────────────

    /// Samples the process tree via the tree monitor.
    ///
    /// Returns `None` if the tree monitor has not been initialized
    /// (connection not set) or the root process is gone.
    pub(crate) fn sample_tree(&self) -> Option<catenary_proc::TreeSnapshot> {
        self.tree_monitor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .map(catenary_proc::TreeMonitor::sample)
    }

    // ── Document versions (publish staleness gate) ────────────────

    /// Records the latest document version sent to the server for `uri`
    /// (`didOpen`/`didChange`) — the reference point for the publish
    /// staleness gate (bug 101, heard-stale leg). Callers issue versions
    /// monotonically per URI across close/reopen
    /// ([`super::client::LspClient::open_document`]), so "older than this"
    /// identifies a straggler from a previous round.
    pub(crate) fn note_doc_version(&self, uri: &str, version: i32) {
        self.doc_versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(uri.to_string(), version);
    }

    /// The latest document version sent for `uri`, if any.
    fn doc_version(&self, uri: &str) -> Option<i32> {
        self.doc_versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(uri)
            .copied()
    }

    /// The cached diagnostics for `uri` **only when a publish settles the
    /// current debt** — the version-aware settlement consult (diagnostics-debt
    /// 03, retiring the version-blind read, bug 85).
    ///
    /// A debt is the answer owed for the version stamped on `uri`'s current sync
    /// state ([`Self::doc_version`] — the last-sent version, the real monotonic
    /// document version of ticket 01's held-open registry). A publish settles it
    /// only when it echoes that version; settlement, not mere presence, is what
    /// makes a cached entry authoritative:
    ///
    /// - a **versioned** publish echoing the current version settles — `Some` of
    ///   its diagnostics, **including `Some(vec![])`**: a versioned empty at the
    ///   current version is the authoritative clean (misc 153's heard-empty
    ///   demoted to exactly this case). A versioned publish carrying any *other*
    ///   version does not settle (`None`): the `< current` straggler is already
    ///   dropped before the cache by the staleness gate in [`Self::on_notification`],
    ///   and a `> current` echo cannot legitimately arise (Catenary owns the
    ///   version sequence);
    /// - an **unversioned** publish is a *hint*, interpreted through the server's
    ///   discipline (ticket 04 makes discipline manifest data; until then it is
    ///   sourced from the conformance profile — here [`Self::declares_push`]):
    ///   - a non-empty unversioned publish carries real findings, so it settles
    ///     with that content (a dirty file must render dirty — the fast native
    ///     publish of a native-then-flycheck server, rust-analyzer's bug-28
    ///     shape, and every unversioned push server's diagnostics);
    ///   - an empty unversioned publish settles **only** for a declared-push
    ///     server, whose contract is a publish on every `didOpen` with an
    ///     explicit `[]` for clean (misc 187 — lattice's authoritative empty).
    ///     For any other server an unversioned empty is the demoted misc-153
    ///     case — it settles nothing (the gopls pull-mode placeholder-push
    ///     defeat, bug 87, stops mattering structurally: a placeholder without a
    ///     current-version echo never renders `[clean]` from authority).
    ///
    /// `None` means the debt is **unsettled** — never-heard, or heard only a
    /// non-settling hint. The caller resolves it through the never-heard path
    /// (pull / probe / evidence bar / the silent-server contract), never a
    /// fabricated `[clean]`. Distinct from [`super::client::LspClient::get_diagnostics`],
    /// which is the raw channel-state read (any cached publish → `Some`) the
    /// retrieval evidence bar consults for heard-ness.
    pub(crate) fn settled_diagnostics(&self, uri: &str) -> Option<Vec<Value>> {
        // Read the debt's version first, then the cache — the same
        // doc_versions-before-diagnostics lock order `on_notification` uses, so
        // the two locks are never held in conflicting orders.
        let current = self.doc_version(uri);
        // Take a copy of the cached entry and release the cache lock before the
        // settlement branch (no work held under the lock).
        let (published_version, diags) = {
            let cache = self
                .diagnostics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.get(uri).cloned()?
        };
        match published_version {
            // Versioned: settles iff it echoes the current sent version.
            Some(pv) => (current == Some(pv)).then_some(diags),
            // Unversioned non-empty: a hint carrying real content — settle with
            // it. Unversioned empty: authoritative only under the declared-push
            // contract, else a non-settling hint.
            None if !diags.is_empty() || self.declares_push => Some(diags),
            None => None,
        }
    }

    // ── Dispatch methods (moved from ServerInbox) ─────────────────

    /// Handles a server notification (no response needed).
    #[allow(clippy::too_many_lines, reason = "match dispatcher with per-arm logic")]
    pub fn on_notification(&self, method: &str, params: &Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Some(uri) = extract::publish_diagnostics_uri(params) else {
                    debug!("publishDiagnostics missing uri");
                    return;
                };
                let version = extract::publish_diagnostics_version(params);
                let diagnostics = extract::publish_diagnostics_diagnostics(params);

                debug!(
                    "Received {} diagnostics for {:?} (version={:?})",
                    diagnostics.len(),
                    uri,
                    version,
                );

                // Track whether server provides version in diagnostics
                if version.is_some() && !self.publishes_version.swap(true, Ordering::SeqCst) {
                    self.capability_notify.notify_waiters();
                }

                // Any publish on this connection is proof of push capability —
                // it arms the retrieval evidence bar for never-heard files
                // (bug 99 residual / misc 156). A stale publish (below) still
                // counts: staleness disqualifies the content, not the channel.
                self.ever_published.store(true, Ordering::SeqCst);

                // Publish staleness gate (bug 101, heard-stale leg): a publish
                // computed against an OLDER document version than the one last
                // sent for this URI is a straggler from a previous round —
                // e.g. an in-flight flycheck result for content that has since
                // been rewritten, landing after the next round's
                // clear-then-open. Caching it would overwrite fresher evidence
                // and read as "heard" for content the server never analyzed,
                // so the receipt would carry the stale shape (the macOS
                // flycheck_multi_round incident, CI run 29091745917). Dropped
                // before the cache; version-less publishes are untouched.
                if let (Some(published), Some(current)) = (version, self.doc_version(uri))
                    && published < current
                {
                    debug!(
                        "Dropping stale publishDiagnostics for {uri}: \
                         version {published} < current {current}",
                    );
                    return;
                }

                let mut cache = self
                    .diagnostics
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.insert(uri.to_string(), (version, diagnostics));
                drop(cache);

                // Bump generation counter and wake waiters
                let mut generations = self
                    .diagnostics_generation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let counter = generations.entry(uri.to_string()).or_insert(0);
                *counter += 1;
                drop(generations);
                self.diagnostics_notify.notify_waiters();
            }
            "$/progress" => {
                let Some(token_value) = extract::progress_token(params) else {
                    debug!("$/progress missing token");
                    return;
                };
                // A terminal server produces no more progress; ignore the event
                // rather than repopulate the registry `on_shutdown` cleared.
                if self.lifecycle().is_terminal() {
                    return;
                }
                let token_str = token_value
                    .as_str()
                    .map_or_else(|| token_value.to_string(), str::to_string);

                let mut tracker = self
                    .progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                tracker.update(&token_str, &params["value"]);

                if tracker.broadcast_changed()
                    && let Some(p) = tracker.primary_progress()
                {
                    debug!("Progress: {} {}%", p.title, p.percentage.unwrap_or(0));
                }

                // Persist progress to DB + snapshot.
                let primary = tracker.primary_progress();
                let db_title = primary.map(|p| p.title.clone());
                let db_message = primary.and_then(|p| p.message.clone());
                let db_pct = primary.and_then(|p| p.percentage);
                // Snapshot the registry counts under the same lock so the
                // lifecycle derives from the single token registry (misc 200):
                // `begun` → Busy, else `created` → Pending, else Healthy.
                let begun = tracker.begun_count();
                let created = tracker.created_count();
                drop(tracker);
                self.persist_progress(db_title.as_deref(), db_message.as_deref(), db_pct);

                // `begin` arms the runtime "sends progress" capability
                // independently of create — a sloppy server that begins a
                // token it never announced still counts (misc 200). Only the
                // begin event flips `ever_busy`; a bare create does not.
                let kind = params["value"]["kind"].as_str();
                if kind == Some("begin") && !self.ever_busy.swap(true, Ordering::SeqCst) {
                    self.capability_notify.notify_waiters();
                }
                if kind == Some("end") && begun == 0 && created == 0 {
                    debug!("Server ready (progress completed)");
                }

                self.apply_progress_lifecycle(begun, created);
                self.progress_notify.notify_waiters();
            }
            "window/logMessage" | "window/showMessage" => {
                if let Some(message) = params.get("message").and_then(|m| m.as_str()) {
                    debug!("LSP server message: {}", message);
                    self.persist_message(message_type_str(params), message);
                }
            }
            _ => {
                trace!("Ignoring notification: {} params={}", method, params);
            }
        }
    }

    /// Handles a server request (response required).
    ///
    /// Returns `Ok(result)` for a success response or `Err(RpcError)`
    /// for an error response. Connection builds the JSON-RPC envelope.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError`] for unsupported methods.
    pub fn on_request(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "workspace/configuration" => {
                let items = params.get("items").and_then(Value::as_array);
                let item_count = items.map_or(1, Vec::len);
                let results: Vec<Value> = (0..item_count)
                    .map(|i| {
                        let item = items.and_then(|arr| arr.get(i));
                        let section = item
                            .and_then(|it| it.get("section"))
                            .and_then(Value::as_str);
                        let scope_uri = item
                            .and_then(|it| it.get("scopeUri"))
                            .and_then(Value::as_str);
                        self.resolve_configuration(section, scope_uri)
                    })
                    .collect();
                Ok(Value::Array(results))
            }
            "client/registerCapability" => {
                self.handle_register_capability(params);
                Ok(Value::Null)
            }
            "client/unregisterCapability" => {
                self.handle_unregister_capability(params);
                Ok(Value::Null)
            }
            // window/workDoneProgress/create: the server announces a work-done
            // token BEFORE it opens the `$/progress` bracket. Register the
            // token in the single progress registry so settle holds on the
            // announced-but-not-started (Pending) state — the elm cold-download
            // gap the create straddles (misc 200) — then ack `Null` (the ack
            // itself is spec-correct and unchanged; the token retires via the
            // `$/progress` `end`, not a wire response). A malformed/missing
            // token still acks `Null` and registers nothing.
            "window/workDoneProgress/create" => {
                // A terminal server produces no more progress; ack `Null` but
                // do not repopulate the registry `on_shutdown` cleared.
                if let Some(token_value) =
                    extract::progress_token(params).filter(|_| !self.lifecycle().is_terminal())
                {
                    let token_str = token_value
                        .as_str()
                        .map_or_else(|| token_value.to_string(), str::to_string);
                    let (begun, created) = {
                        let mut tracker = self
                            .progress
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        tracker.create(&token_str);
                        (tracker.begun_count(), tracker.created_count())
                    };
                    self.apply_progress_lifecycle(begun, created);
                    self.progress_notify.notify_waiters();
                } else {
                    debug!("window/workDoneProgress/create missing token");
                }
                Ok(Value::Null)
            }
            // workspace/diagnostic/refresh: server asks client to re-pull
            // diagnostics for open documents. No-op — document lifecycle
            // is transient (open/settle/save/settle/retrieve/close within
            // done_editing), so there is no stale cache to invalidate.
            // Mid-batch, the settle pipeline already waits for quiescence.
            "window/showMessageRequest" | "workspace/diagnostic/refresh" => Ok(Value::Null),
            _ => Err(RpcError {
                code: -32601,
                message: format!("Method '{method}' not supported by client"),
            }),
        }
    }

    /// Handles reader loop shutdown (server connection lost).
    ///
    /// Called after the `alive` flag is set to `false`. Updates internal
    /// state and wakes any waiters blocked on diagnostics or state changes.
    pub fn on_shutdown(&self) {
        self.set_lifecycle(ServerLifecycle::Dead);
        if let Ok(mut progress) = self.progress.lock() {
            progress.clear();
        }
        self.config_change_registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.watched_files_registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.diagnostics_notify.notify_waiters();
    }

    /// Transitions from `Probing` to `Healthy` if currently probing.
    ///
    /// No-op if the server is in any other state. Used by tool request
    /// success and the health probe to mark the server as proven.
    pub fn try_transition_probing_to_healthy(&self) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *lifecycle == ServerLifecycle::Probing {
            *lifecycle = ServerLifecycle::Healthy;
            drop(lifecycle);
            self.state_notify.notify_waiters();
        }
    }

    /// Whether the server is actively reporting progress.
    ///
    /// Used by `Connection::request` to pause failure detection budget
    /// drain during explained work (e.g., indexing, flycheck).
    ///
    /// Reads the authoritative `Busy(n)` lifecycle — the same open-bracket
    /// signal `await_idle` uses — rather than `try_lock`ing the progress
    /// tracker. A prior `try_lock` fail-safe returned `true` on lock
    /// *contention* (not just genuine progress), which could spuriously pause
    /// `request()`'s stuck-server budget and stop it self-bounding. The
    /// lifecycle lock is only ever held briefly, so this reads the real state.
    pub fn is_progress_active(&self) -> bool {
        matches!(self.lifecycle(), ServerLifecycle::Busy(_))
    }

    /// Returns a reference to the state-change notifier.
    ///
    /// Used by `Connection::request` to wait for server settle after
    /// `ContentModified` instead of a fixed sleep.
    pub fn state_notify(&self) -> &Notify {
        &self.state_notify
    }

    // ── Dynamic registration accessors ────────────────────────────

    /// Returns whether the server has any active dynamic registrations
    /// for `workspace/didChangeConfiguration` notifications.
    ///
    /// Used by the manager to decide whether to push configuration
    /// changes to this server (e.g., on `/add-dir`).
    pub fn wants_did_change_configuration(&self) -> bool {
        !self
            .config_change_registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Snapshot of all currently-registered file watchers across registrations.
    ///
    /// Clones the parsed watchers under the lock so callers can match without
    /// holding it during I/O. Consumed by the WS31 conditional nudge.
    pub(crate) fn watched_files_snapshot(&self) -> Vec<ParsedWatcher> {
        self.watched_files_registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    /// Parses `client/registerCapability` params and stores
    /// registrations for supported methods.
    fn handle_register_capability(&self, params: &Value) {
        let Some(registrations) = params.get("registrations").and_then(Value::as_array) else {
            return;
        };

        for reg in registrations {
            let Some(method) = reg.get("method").and_then(Value::as_str) else {
                debug!("registration entry missing 'method' field");
                continue;
            };

            if method == "workspace/didChangeConfiguration" {
                let id = reg
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.config_change_registrations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id);
                debug!("server registered for workspace/didChangeConfiguration");
                continue;
            }

            if method == "workspace/didChangeWatchedFiles" {
                let id = reg
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                let watchers = reg
                    .get("registerOptions")
                    .and_then(|opts| opts.get("watchers"))
                    .and_then(Value::as_array);

                let Some(watchers) = watchers else {
                    debug!("watched-files registration {id} missing registerOptions.watchers");
                    continue;
                };

                let mut parsed = Vec::with_capacity(watchers.len());
                for watcher in watchers {
                    if let Some(w) = ParsedWatcher::from_value(watcher) {
                        parsed.push(w);
                    } else {
                        debug!("skipping malformed watcher in registration {id}");
                    }
                }

                self.watched_files_registrations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, parsed);
                debug!("server registered for workspace/didChangeWatchedFiles");
            }
        }
    }

    /// Parses `client/unregisterCapability` params and removes
    /// registrations by ID.
    fn handle_unregister_capability(&self, params: &Value) {
        // Note: the LSP spec misspells "unregisterations" — this is normative.
        let Some(unregistrations) = params.get("unregisterations").and_then(Value::as_array) else {
            return;
        };

        for unreg in unregistrations {
            let Some(method) = unreg.get("method").and_then(Value::as_str) else {
                debug!("unregistration entry missing 'method' field");
                continue;
            };

            if method == "workspace/didChangeConfiguration"
                && let Some(id) = unreg.get("id").and_then(Value::as_str)
            {
                self.config_change_registrations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(id);
                debug!("server unregistered from workspace/didChangeConfiguration");
            }

            if method == "workspace/didChangeWatchedFiles"
                && let Some(id) = unreg.get("id").and_then(Value::as_str)
            {
                self.watched_files_registrations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(id);
                debug!("server unregistered from workspace/didChangeWatchedFiles");
            }
        }
    }
}

/// Maps an LSP `window/logMessage` / `window/showMessage` `type` field to a
/// severity tag for the snapshot's `last_message`.
///
/// `MessageType`: 1 = Error, 2 = Warning, 3 = Info, 4 = Log, 5 = Debug.
/// An absent or unrecognized type defaults to `"info"`.
fn message_type_str(params: &Value) -> &'static str {
    match params.get("type").and_then(Value::as_u64) {
        Some(1) => "error",
        Some(2) => "warning",
        Some(4) => "log",
        Some(5) => "debug",
        _ => "info",
    }
}

/// Resolves a `workspace/configuration` section path against settings.
///
/// Splits `section` on `.` and traverses the JSON object tree.
/// Returns `{}` if settings are `None`, section is `None`, or the path
/// doesn't match.
fn resolve_section(settings: Option<&Value>, section: Option<&str>) -> Value {
    let (Some(mut current), Some(section)) = (settings, section) else {
        return Value::Object(serde_json::Map::new());
    };
    for key in section.split('.') {
        match current.get(key) {
            Some(child) => current = child,
            None => return Value::Object(serde_json::Map::new()),
        }
    }
    current.clone()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::super::instance_key::Scope;
    use super::*;
    use crate::logging::LoggingServer;
    use crate::logging::test_support::{query_all_messages, setup_logging};
    use serde_json::json;
    use std::time::Duration;

    fn test_server() -> LspServer {
        LspServer::new("test".to_string(), "test-server".to_string(), None)
    }

    /// Helper: creates an `LspServer` with capabilities already set.
    fn server_with_caps(caps: Value) -> LspServer {
        let server = test_server();
        server.set_capabilities(caps);
        server
    }

    /// Helper: creates a **blessed** `LspServer` with capabilities already set.
    ///
    /// `test-server` is an unverified name, so it classifies enrichment-only and
    /// [`LspServer::supports_diagnostics`] is `false` regardless of advertised
    /// capability (diagnostics-debt 04b). The push/pull OR-logic tests below need
    /// a diagnostics-eligible server, so they use `clangd` — blessed and casing-free
    /// (its misc-196 row is plain `event`: not pull-suppressed, not enrichment-only,
    /// and neither scan nor diff).
    fn blessed_server_with_caps(caps: Value) -> LspServer {
        let server = LspServer::new("c".to_string(), "clangd".to_string(), None);
        server.set_capabilities(caps);
        server
    }

    #[test]
    fn scan_and_diff_casing_projects_from_the_personas() {
        // misc 196: the round-conditional disciplines reach the retrieval seams via
        // the `LspServer` casing flags, set once at construction from the profile.
        // The scan/diff personas project them; every other discipline (and the
        // unverified `test-server`) is neither. The static `owes_answer` stays
        // false for scan/diff — their arms are round-conditional, read at the seam.
        let scan = LspServer::new("mockls-scan".to_string(), "mockls-scan".to_string(), None);
        assert!(scan.is_scan(), "the scan persona casts is_scan");
        assert!(!scan.is_diff());
        assert!(
            !scan.owes_answer(),
            "scan owes nothing to the static contract"
        );

        let diff = LspServer::new("mockls-diff".to_string(), "mockls-diff".to_string(), None);
        assert!(diff.is_diff(), "the diff persona casts is_diff");
        assert!(!diff.is_scan());
        assert!(
            !diff.owes_answer(),
            "diff owes nothing to the static contract"
        );

        // A declared-push persona (lattice shape) and an unverified name are neither.
        let declared = LspServer::new("m".to_string(), "mockls-declared".to_string(), None);
        assert!(!declared.is_scan() && !declared.is_diff());
        assert!(
            declared.owes_answer(),
            "declared-push still owes statically"
        );

        let unverified = test_server();
        assert!(!unverified.is_scan() && !unverified.is_diff());
        assert!(!unverified.owes_answer());
    }

    // ── ParsedWatcher::covers tests (WS31 changed-set routing) ────────

    #[test]
    fn parsed_watcher_covers_glob_and_kind() {
        // Default kind (all 7) watcher on `**/*.rs`.
        let w =
            ParsedWatcher::from_value(&json!({ "globPattern": "**/*.rs" })).expect("valid watcher");
        let rel = std::path::Path::new("src/lib.rs");
        let abs = std::path::Path::new("/root/src/lib.rs");
        assert!(w.covers(rel, abs, ChangeKind::Created));
        assert!(w.covers(rel, abs, ChangeKind::Changed));

        // A non-matching extension is not covered.
        let toml_rel = std::path::Path::new("Cargo.toml");
        let toml_abs = std::path::Path::new("/root/Cargo.toml");
        assert!(!w.covers(toml_rel, toml_abs, ChangeKind::Changed));
    }

    #[test]
    fn parsed_watcher_covers_kind_mask_filters() {
        // Watcher registered with Change-only (kind 2) suppresses creations.
        let w = ParsedWatcher::from_value(&json!({ "globPattern": "**/*.rs", "kind": 2 }))
            .expect("valid watcher");
        let rel = std::path::Path::new("src/lib.rs");
        let abs = std::path::Path::new("/root/src/lib.rs");
        assert!(
            !w.covers(rel, abs, ChangeKind::Created),
            "Change-only watcher must not cover a Created candidate"
        );
        assert!(
            w.covers(rel, abs, ChangeKind::Changed),
            "Change-only watcher covers a Changed candidate"
        );

        // Create-only (kind 1) suppresses changes.
        let w = ParsedWatcher::from_value(&json!({ "globPattern": "**/*.rs", "kind": 1 }))
            .expect("valid watcher");
        assert!(w.covers(rel, abs, ChangeKind::Created));
        assert!(!w.covers(rel, abs, ChangeKind::Changed));
    }

    #[test]
    fn ws31_review_c2_watcher_star_no_cross_segment() {
        // A single-`*` segment-scoped pattern must NOT cross `/` (LSP 3.17
        // semantics, `literal_separator(true)`). Today `ParsedWatcher` compiles
        // with globset's default `literal_separator(false)`, so `*` crosses
        // segments and the nested path is wrongly covered.
        let w =
            ParsedWatcher::from_value(&json!({ "globPattern": "*.json" })).expect("valid watcher");

        // Top-level file matches.
        let top_rel = std::path::Path::new("b.json");
        let top_abs = std::path::Path::new("/root/b.json");
        assert!(
            w.covers(top_rel, top_abs, ChangeKind::Changed),
            "*.json must cover a top-level b.json"
        );

        // Nested file must NOT match — `*` does not cross a segment boundary.
        let nested_rel = std::path::Path::new("a/b.json");
        let nested_abs = std::path::Path::new("/root/a/b.json");
        assert!(
            !w.covers(nested_rel, nested_abs, ChangeKind::Changed),
            "*.json must NOT cover a nested a/b.json (single * does not cross /)"
        );
    }

    #[test]
    fn ws31_review_c2_watcher_baseuri_percent_decoded() {
        // A relative-pattern watcher whose baseUri is percent-encoded must
        // decode to the real path so its base prefix strips against the actual
        // absolute path. Today `uri_to_path` leaves the literal `%20`, so the
        // strip_prefix fails and nothing matches.
        let w = ParsedWatcher::from_value(&json!({
            "globPattern": {
                "baseUri": "file:///home/u/my%20project",
                "pattern": "**/*.rs"
            }
        }))
        .expect("valid watcher");

        let rel = std::path::Path::new("src/lib.rs");
        let abs = std::path::Path::new("/home/u/my project/src/lib.rs");
        assert!(
            w.covers(rel, abs, ChangeKind::Changed),
            "baseUri %20 must decode to a space so the base prefix matches"
        );
    }

    #[test]
    fn ws31_review_d_object_watcher_no_baseuri_degrades() {
        // An object-form globPattern with NO baseUri must degrade gracefully to
        // a Plain workspace-relative watcher on its `pattern` (the pre-C2
        // behavior). C2 routed every object through `GlobPattern::from_value`,
        // which `Err`s on a missing baseUri, so `.ok()?` drops the WHOLE
        // watcher. RED: returns None / no covering watcher.
        let w = ParsedWatcher::from_value(&json!({
            "globPattern": { "pattern": "**/*.rs" }
        }))
        .expect("object watcher with no baseUri should degrade to workspace-relative");

        let rel = std::path::Path::new("foo.rs");
        let abs = std::path::Path::new("/root/foo.rs");
        assert!(
            w.covers(rel, abs, ChangeKind::Changed),
            "degraded workspace-relative watcher must cover a matching foo.rs"
        );

        // A non-`file://` baseUri likewise degrades to workspace-relative on its
        // pattern rather than dropping the watcher.
        let w = ParsedWatcher::from_value(&json!({
            "globPattern": { "baseUri": "vscode-vfs://host/proj", "pattern": "**/*.rs" }
        }))
        .expect("object watcher with non-file:// baseUri should degrade");
        assert!(
            w.covers(rel, abs, ChangeKind::Changed),
            "non-file:// baseUri must degrade to a workspace-relative matcher"
        );
    }

    #[test]
    fn ws31_review_d_object_watcher_uncompilable_pattern_drops() {
        // Graceful degradation must NOT extend to a pattern that cannot compile:
        // an object with no baseUri whose `pattern` is an invalid glob still
        // drops (no degradation to a broken matcher). A bare unclosed `[`
        // character class fails `LspGlob::new`.
        let w = ParsedWatcher::from_value(&json!({
            "globPattern": { "pattern": "[" }
        }));
        assert!(
            w.is_none(),
            "an object watcher whose pattern won't compile must drop, not degrade"
        );
    }

    // ── Identity accessor tests ──────────────────────────────────────

    #[test]
    fn test_lsp_server_accessors() {
        let server = LspServer::new("rust".to_string(), "rust-analyzer".to_string(), None);
        assert_eq!(server.language_id(), "rust");
        assert_eq!(server.server_name(), "rust-analyzer");
        assert!(server.scope().is_none());
    }

    #[test]
    fn test_lsp_server_scope_set_once() {
        let server = test_server();
        assert!(server.scope().is_none());

        server.set_scope(Scope::Root(std::path::PathBuf::from("/project")));
        assert_eq!(
            server.scope(),
            Some(&Scope::Root(std::path::PathBuf::from("/project")))
        );

        // Second set is a no-op
        server.set_scope(Scope::Root(std::path::PathBuf::from("/other")));
        assert_eq!(
            server.scope(),
            Some(&Scope::Root(std::path::PathBuf::from("/project")))
        );
    }

    #[test]
    fn test_lsp_server_key_construction() {
        let server = LspServer::new("rust".to_string(), "rust-analyzer".to_string(), None);

        // Before set_scope, key() returns None
        assert!(server.key().is_none());

        server.set_scope(Scope::Root(std::path::PathBuf::from("/project")));
        let key = server.key().expect("key should be Some after set_scope");
        assert_eq!(key.language_id, "rust");
        assert_eq!(key.server, "rust-analyzer");
        assert_eq!(key.scope, Scope::Root(std::path::PathBuf::from("/project")));
    }

    // ── Capability tests ──────────────────────────────────────────

    #[test]
    fn set_capabilities_extracts_pull_diagnostics() {
        let server =
            server_with_caps(json!({ "diagnosticProvider": { "interFileDependencies": true } }));
        assert!(server.supports_pull_diagnostics());
    }

    #[test]
    fn no_diagnostic_provider() {
        let server = server_with_caps(json!({}));
        assert!(!server.supports_pull_diagnostics());
    }

    #[test]
    fn pull_suppressed_server_never_reports_pull_support() {
        // rust-analyzer is cased to suppress pull (misc 157). Even when it
        // spontaneously advertises `diagnosticProvider`, pull stays off — the
        // client-side pull path is structurally unreachable.
        let server = LspServer::new("rust".to_string(), "rust-analyzer".to_string(), None);
        assert!(server.pull_suppressed());
        server.set_capabilities(json!({ "diagnosticProvider": { "interFileDependencies": true } }));
        assert!(
            !server.supports_pull_diagnostics(),
            "a pull-suppressed server must never report pull support",
        );
    }

    #[test]
    fn uncased_server_is_not_pull_suppressed() {
        let server = LspServer::new("go".to_string(), "gopls".to_string(), None);
        assert!(!server.pull_suppressed());
        server.set_capabilities(json!({ "diagnosticProvider": {} }));
        assert!(server.supports_pull_diagnostics());
    }

    #[test]
    fn declared_push_server_carries_declaration_before_any_publish() {
        // lattice is cased declared-push (misc 187): the declaration is
        // construction-time state, present on a fresh connection with zero
        // publishes heard — exactly the window where per-connection
        // demonstration (`has_ever_published`) cannot arm the evidence bar.
        let server = LspServer::new("markdown".to_string(), "lattice".to_string(), None);
        assert!(server.declares_push());
        assert!(!server.has_ever_published());
    }

    #[test]
    fn uncased_server_does_not_declare_push() {
        let server = LspServer::new("go".to_string(), "gopls".to_string(), None);
        assert!(!server.declares_push());
    }

    #[test]
    fn before_set_capabilities_nothing_supported() {
        let server = test_server();
        assert!(!server.supports_pull_diagnostics());
        assert!(!server.supports_workspace_symbols());
        // capabilities() returns empty object
        assert_eq!(server.capabilities(), &json!({}));
    }

    #[test]
    fn lifecycle_starts_initializing() {
        let server = test_server();
        assert_eq!(server.lifecycle(), ServerLifecycle::Initializing);
        assert!(!server.sends_progress());
    }

    #[test]
    fn set_lifecycle_transitions_and_notifies() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        assert_eq!(server.lifecycle(), ServerLifecycle::Healthy);

        server.set_lifecycle(ServerLifecycle::Dead);
        assert_eq!(server.lifecycle(), ServerLifecycle::Dead);
    }

    #[test]
    fn set_lifecycle_warns_on_first_terminal_transition_only() {
        let (_logging, db, _guard) = setup_logging();

        // Server needs a scope so key() returns Some — required for
        // the warn branch.
        let server = LspServer::new("rust".to_string(), "rust-analyzer".to_string(), None);
        server.set_scope(Scope::Root(std::path::PathBuf::from("/project")));

        // Non-terminal → terminal: should emit warn
        server.set_lifecycle(ServerLifecycle::Healthy);
        server.set_lifecycle(ServerLifecycle::Dead);

        let warn_count = query_all_messages(&db)
            .iter()
            .filter(|m| m.level == "warn")
            .count();
        assert_eq!(
            warn_count, 1,
            "expected exactly one warn on first terminal transition"
        );

        // Terminal → terminal: should NOT emit another warn
        server.set_lifecycle(ServerLifecycle::Dead);

        let warn_count = query_all_messages(&db)
            .iter()
            .filter(|m| m.level == "warn")
            .count();
        assert_eq!(
            warn_count, 1,
            "no additional warn on terminal-to-terminal transition"
        );
    }

    #[test]
    fn supports_capability_true() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": true }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_false() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": false }));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_options_object() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": {} }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_detailed_options() {
        let server = server_with_caps(json!({
            "workspaceSymbolProvider": { "resolveProvider": true }
        }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_missing() {
        let server = server_with_caps(json!({}));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_null() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": null }));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn explicit_false_not_supported() {
        let server = server_with_caps(json!({
            "definitionProvider": false,
            "referencesProvider": false,
            "documentSymbolProvider": false,
            "workspaceSymbolProvider": false,
            "renameProvider": false,
            "typeDefinitionProvider": false,
            "implementationProvider": false,
            "callHierarchyProvider": false,
            "typeHierarchyProvider": false,
            "codeActionProvider": false,
        }));
        assert!(!server.supports_definition());
        assert!(!server.supports_references());
        assert!(!server.supports_document_symbols());
        assert!(!server.supports_workspace_symbols());
        assert!(!server.supports_rename());
        assert!(!server.supports_type_definition());
        assert!(!server.supports_implementation());
        assert!(!server.supports_call_hierarchy());
        assert!(!server.supports_type_hierarchy());
        assert!(!server.supports_code_action());
    }

    #[test]
    fn empty_capabilities_nothing_supported() {
        let server = server_with_caps(json!({}));
        assert!(!server.supports_definition());
        assert!(!server.supports_references());
        assert!(!server.supports_document_symbols());
        assert!(!server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
        assert!(!server.supports_rename());
        assert!(!server.supports_type_definition());
        assert!(!server.supports_implementation());
        assert!(!server.supports_call_hierarchy());
        assert!(!server.supports_type_hierarchy());
        assert!(!server.supports_code_action());
    }

    #[test]
    fn supports_all_capabilities() {
        let server = server_with_caps(json!({
            "definitionProvider": true,
            "referencesProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": { "resolveProvider": true },
            "renameProvider": true,
            "typeDefinitionProvider": true,
            "implementationProvider": true,
            "callHierarchyProvider": true,
            "typeHierarchyProvider": true,
            "codeActionProvider": true,
        }));
        assert!(server.supports_definition());
        assert!(server.supports_references());
        assert!(server.supports_document_symbols());
        assert!(server.supports_workspace_symbols());
        assert!(server.supports_workspace_symbol_resolve());
        assert!(server.supports_rename());
        assert!(server.supports_type_definition());
        assert!(server.supports_implementation());
        assert!(server.supports_call_hierarchy());
        assert!(server.supports_type_hierarchy());
        assert!(server.supports_code_action());
    }

    // ── Workspace symbol resolve ───────────────────────────────────

    #[test]
    fn workspace_symbol_resolve_boolean_provider() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": true }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_empty_options() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": {} }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_false() {
        let server = server_with_caps(json!({
            "workspaceSymbolProvider": { "resolveProvider": false }
        }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_true() {
        let server = server_with_caps(json!({
            "workspaceSymbolProvider": { "resolveProvider": true }
        }));
        assert!(server.supports_workspace_symbols());
        assert!(server.supports_workspace_symbol_resolve());
    }

    // ── resolve_section tests (moved from inbox.rs) ───────────────

    #[test]
    fn resolve_section_traverses_dot_path() {
        let settings = json!({
            "python": {
                "analysis": {
                    "exclude": ["**/target"],
                    "extraPaths": []
                },
                "pythonPath": "/usr/bin/python3"
            }
        });
        assert_eq!(
            resolve_section(Some(&settings), Some("python.analysis")),
            json!({"exclude": ["**/target"], "extraPaths": []})
        );
        assert_eq!(
            resolve_section(Some(&settings), Some("python.pythonPath")),
            json!("/usr/bin/python3")
        );
        assert_eq!(
            resolve_section(Some(&settings), Some("python")),
            json!({"analysis": {"exclude": ["**/target"], "extraPaths": []}, "pythonPath": "/usr/bin/python3"})
        );
    }

    #[test]
    fn resolve_section_missing_path_returns_empty_object() {
        let settings = json!({"python": {"analysis": {}}});
        assert_eq!(resolve_section(Some(&settings), Some("rust")), json!({}));
        assert_eq!(
            resolve_section(Some(&settings), Some("python.nonexistent")),
            json!({})
        );
    }

    #[test]
    fn resolve_section_none_settings_returns_empty_object() {
        assert_eq!(resolve_section(None, Some("python")), json!({}));
    }

    #[test]
    fn resolve_section_none_section_returns_empty_object() {
        let settings = json!({"python": {}});
        assert_eq!(resolve_section(Some(&settings), None), json!({}));
    }

    // ── on_request tests (moved from inbox.rs) ────────────────────

    #[test]
    fn configuration_request_uses_settings() {
        let server = LspServer::new(
            "test".to_string(),
            "test-server".to_string(),
            Some(json!({"mockls": {"key": "value"}})),
        );
        let result = server
            .on_request(
                "workspace/configuration",
                &json!({"items": [{"section": "mockls"}]}),
            )
            .expect("configuration request should succeed");
        assert_eq!(result, json!([{"key": "value"}]));
    }

    #[test]
    fn configuration_request_without_settings_returns_empty_objects() {
        let server = test_server();
        let result = server
            .on_request(
                "workspace/configuration",
                &json!({"items": [{"section": "mockls"}, {"section": "other"}]}),
            )
            .expect("configuration request should succeed");
        assert_eq!(result, json!([{}, {}]));
    }

    #[test]
    fn register_capability_accepted() {
        let server = test_server();
        let result = server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{"id": "1", "method": "textDocument/didChangeConfiguration"}]}),
            )
            .expect("registerCapability should succeed");
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn unregister_capability_accepted() {
        let server = test_server();
        let result = server
            .on_request(
                "client/unregisterCapability",
                &json!({"unregisterations": [{"id": "1", "method": "textDocument/didChangeConfiguration"}]}),
            )
            .expect("unregisterCapability should succeed");
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn show_message_request_accepted() {
        let server = test_server();
        let result = server
            .on_request(
                "window/showMessageRequest",
                &json!({"type": 1, "message": "Restart?", "actions": [{"title": "Yes"}]}),
            )
            .expect("showMessageRequest should succeed");
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn unknown_request_rejected() {
        let server = test_server();
        let err = server
            .on_request("custom/unknownMethod", &json!({}))
            .expect_err("unknown method should be rejected");
        assert_eq!(err.code, -32601);
    }

    // ── on_notification tests (moved from inbox.rs) ───────────────

    #[test]
    fn is_progress_active_begin_end() {
        let server = test_server();
        assert!(!server.is_progress_active());

        // Progress begin
        server.on_notification(
            "$/progress",
            &json!({
                "token": "test-token",
                "value": { "kind": "begin", "title": "Indexing", "percentage": 0 }
            }),
        );
        assert!(server.is_progress_active());

        // Progress end
        server.on_notification(
            "$/progress",
            &json!({
                "token": "test-token",
                "value": { "kind": "end" }
            }),
        );
        assert!(!server.is_progress_active());
    }

    #[test]
    fn publish_diagnostics_updates_cache_and_generation() {
        let server = test_server();
        assert!(
            !server.has_ever_published(),
            "a fresh connection has heard no publish"
        );

        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": "file:///test.rs",
                "diagnostics": [{"message": "unused variable", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}}]
            }),
        );

        let cache = server.diagnostics.lock().expect("lock");
        assert!(cache.contains_key("file:///test.rs"));
        let (version, diags) = cache.get("file:///test.rs").expect("entry");
        assert!(version.is_none());
        assert_eq!(diags.len(), 1);
        drop(cache);

        let generations = server.diagnostics_generation.lock().expect("lock");
        assert_eq!(generations.get("file:///test.rs").copied(), Some(1));
        drop(generations);

        // Any publish arms the retrieval evidence bar (bug 99 residual).
        assert!(server.has_ever_published());
    }

    #[test]
    fn stale_versioned_publish_is_gated_fresh_survives() {
        // The publish staleness gate (bug 101, heard-stale leg): a publish
        // carrying an older version than the one last sent for the URI is a
        // straggler from a previous round — dropped before the cache, so it
        // can never overwrite fresher evidence or read as "heard" for
        // content the server never analyzed. It still proves push
        // capability (`has_ever_published`).
        let server = test_server();
        let uri = "file:///test.rs";
        server.note_doc_version(uri, 2);

        // Fresh publish (version matches the last-sent version): cached.
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": uri,
                "version": 2,
                "diagnostics": [{"message": "fresh", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );

        // Straggler from the previous round (version 1 < 2): dropped.
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": uri,
                "version": 1,
                "diagnostics": [{"message": "stale", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );

        let cache = server.diagnostics.lock().expect("lock");
        let (version, diags) = cache.get(uri).expect("fresh entry survives");
        assert_eq!(*version, Some(2), "the fresh publish's version is kept");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0]["message"], "fresh",
            "the straggler must not overwrite the fresh evidence"
        );
        drop(cache);

        // The generation bumped once — the straggler never reached the cache.
        let generations = server.diagnostics_generation.lock().expect("lock");
        assert_eq!(generations.get(uri).copied(), Some(1));
        drop(generations);

        // Staleness disqualifies the content, not the channel.
        assert!(server.has_ever_published());
    }

    #[test]
    fn versionless_publish_is_never_gated() {
        // Version-less publishes (most servers) carry no staleness evidence
        // — the gate must not touch them even when a version was sent.
        let server = test_server();
        let uri = "file:///test.rs";
        server.note_doc_version(uri, 5);

        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": uri,
                "diagnostics": [{"message": "no version", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );

        let cached = server.diagnostics.lock().expect("lock").contains_key(uri);
        assert!(cached, "a version-less publish is cached as before");
    }

    // ── settled_diagnostics: version-echo settlement (diagnostics-debt 03) ──

    /// A versioned publish echoing the current sent version SETTLES the debt —
    /// even when EMPTY (the versioned-empty authoritative clean, misc 153's
    /// heard-empty demoted to exactly this case).
    #[test]
    fn versioned_empty_at_current_version_settles() {
        let server = test_server();
        let uri = "file:///v.rs";
        server.note_doc_version(uri, 3);

        // A versioned empty publish at the current version.
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({ "uri": uri, "version": 3, "diagnostics": [] }),
        );

        // Settlement: Some(empty) — an authoritative clean, distinct from an
        // unsettled None.
        let settled = server.settled_diagnostics(uri);
        assert!(
            matches!(settled, Some(ref d) if d.is_empty()),
            "a versioned empty at the current version settles as an \
             authoritative clean, got: {settled:?}"
        );
        // The raw channel-state read still reports heard.
        let heard = server.diagnostics.lock().expect("lock").contains_key(uri);
        assert!(heard, "the publish is heard");
    }

    /// A versioned non-empty publish at the current version settles with its
    /// diagnostics.
    #[test]
    fn versioned_dirty_at_current_version_settles() {
        let server = test_server();
        let uri = "file:///d.rs";
        server.note_doc_version(uri, 1);
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": uri, "version": 1,
                "diagnostics": [{"message": "boom", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );
        let settled = server.settled_diagnostics(uri).expect("settled");
        assert_eq!(settled.len(), 1, "the dirty verdict settles");
        assert_eq!(settled[0]["message"], "boom");
    }

    /// A stale-version publish NEVER settles a fresh debt (the `1cacf5f` gate,
    /// now native). The staleness gate drops a `< current` publish before the
    /// cache, so the only surviving entry is fresh; but even a defensively
    /// cached older version reads unsettled through `settled_diagnostics`. Here
    /// the debt is bumped to a NEWER version after a publish landed at the old
    /// one — the cached publish no longer echoes the current version.
    #[test]
    fn stale_version_publish_never_settles() {
        let server = test_server();
        let uri = "file:///s.rs";
        // Round 1: send version 1, hear a version-1 publish → cached.
        server.note_doc_version(uri, 1);
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({ "uri": uri, "version": 1, "diagnostics": [] }),
        );
        assert!(
            server.settled_diagnostics(uri).is_some(),
            "at version 1 the version-1 publish settles"
        );

        // Round 2: the document changes — the debt is now version 2, and no
        // new publish has echoed it yet. The version-1 publish still in the
        // cache is stale: it must NOT settle the fresh debt.
        server.note_doc_version(uri, 2);
        assert!(
            server.settled_diagnostics(uri).is_none(),
            "a publish carrying the previous version never settles the fresh \
             debt (bug 85 — the staleness question, now version-aware)"
        );
        // …but the channel still reads heard (a publish did arrive).
        assert!(
            server.diagnostics.lock().expect("lock").contains_key(uri),
            "raw heard-ness is unchanged — only settlement is version-aware"
        );
    }

    /// A late same-version publish RE-SETTLES with better content on the next
    /// consult (the repeat-run contract; replace-per-URI, RA native-then-
    /// flycheck — bug 28). Completeness stays settle's job.
    #[test]
    fn late_same_version_publish_resettles_with_better_content() {
        let server = test_server();
        let uri = "file:///late.rs";
        server.note_doc_version(uri, 4);

        // First (fast native) publish at version 4: a partial verdict.
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": uri, "version": 4,
                "diagnostics": [{"message": "native", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );
        let first = server.settled_diagnostics(uri).expect("first settles");
        assert_eq!(first[0]["message"], "native");

        // A LATER publish at the SAME version 4 (flycheck) replaces per-URI —
        // the next consult re-settles with the better content.
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": uri, "version": 4,
                "diagnostics": [
                    {"message": "native", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}},
                    {"message": "flycheck", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}}
                ]
            }),
        );
        let second = server.settled_diagnostics(uri).expect("re-settles");
        assert_eq!(second.len(), 2, "the late same-version publish re-settles");
        assert_eq!(second[1]["message"], "flycheck");
    }

    /// An UNVERSIONED empty publish from an undeclared server does NOT settle —
    /// it is a hint, not authority (the demoted misc-153 case; the gopls
    /// pull-mode placeholder-push defeat, bug 87, stops mattering structurally).
    #[test]
    fn unversioned_empty_undeclared_does_not_settle() {
        let server = test_server(); // "test-server" — does not declare push
        assert!(!server.declares_push());
        let uri = "file:///u.rs";
        server.note_doc_version(uri, 1);

        // An unversioned empty publish (a placeholder / clearing push).
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({ "uri": uri, "diagnostics": [] }),
        );

        // Heard on the channel, but it settles nothing — the debt stays open.
        assert!(
            server.diagnostics.lock().expect("lock").contains_key(uri),
            "the unversioned empty is heard"
        );
        assert!(
            server.settled_diagnostics(uri).is_none(),
            "an unversioned empty from an undeclared server is a hint, not an \
             authoritative clean — it must not settle (bug 85 / bug 87)"
        );
    }

    /// An UNVERSIONED empty publish from a DECLARED-PUSH server settles as the
    /// authoritative clean — its contract is a publish on every didOpen with an
    /// explicit `[]` for clean (misc 187, lattice's authoritative empty).
    #[test]
    fn unversioned_empty_declared_push_settles() {
        let server = LspServer::new("markdown".to_string(), "lattice".to_string(), None);
        assert!(server.declares_push());
        let uri = "file:///clean.md";
        server.note_doc_version(uri, 1);

        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({ "uri": uri, "diagnostics": [] }),
        );

        let settled = server.settled_diagnostics(uri);
        assert!(
            matches!(settled, Some(ref d) if d.is_empty()),
            "a declared-push server's unversioned empty is the contractual \
             clean and settles, got: {settled:?}"
        );
    }

    /// An UNVERSIONED non-empty publish settles with its content regardless of
    /// declaration — a hint carrying real findings must render dirty (the fast
    /// native publish of a native-then-flycheck server, and every unversioned
    /// push server's diagnostics).
    #[test]
    fn unversioned_dirty_settles_with_content() {
        let server = test_server();
        let uri = "file:///ud.rs";
        server.note_doc_version(uri, 1);
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": uri,
                "diagnostics": [{"message": "real error", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]
            }),
        );
        let settled = server.settled_diagnostics(uri).expect("dirty settles");
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0]["message"], "real error");
    }

    /// A never-heard URI is unsettled (`None`) — no publish, no settlement.
    #[test]
    fn never_heard_uri_is_unsettled() {
        let server = test_server();
        server.note_doc_version("file:///nh.rs", 1);
        assert!(
            server.settled_diagnostics("file:///nh.rs").is_none(),
            "a never-heard URI has no settling publish"
        );
    }

    #[test]
    fn progress_begin_end_updates_lifecycle() {
        let server = test_server();
        assert!(!server.sends_progress());
        assert_eq!(server.lifecycle(), ServerLifecycle::Initializing);
        assert_eq!(server.in_progress_count(), 0);

        // Begin
        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-1",
                "value": { "kind": "begin", "title": "Checking", "percentage": 0 }
            }),
        );
        assert!(server.sends_progress());
        assert_eq!(server.lifecycle(), ServerLifecycle::Busy(1));
        assert_eq!(server.in_progress_count(), 1);

        // Second begin (overlapping token)
        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-2",
                "value": { "kind": "begin", "title": "Indexing", "percentage": 0 }
            }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Busy(2));
        assert_eq!(server.in_progress_count(), 2);

        // End first
        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-1",
                "value": { "kind": "end" }
            }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Busy(1));
        assert_eq!(server.in_progress_count(), 1);

        // End second — transitions to Healthy
        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-2",
                "value": { "kind": "end" }
            }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Healthy);
        assert_eq!(server.in_progress_count(), 0);
    }

    #[test]
    fn progress_ignored_in_terminal_state() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Dead);

        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-1",
                "value": { "kind": "begin", "title": "Checking", "percentage": 0 }
            }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Dead);
    }

    // ── window/workDoneProgress/create → Pending lifecycle (misc 200) ──

    #[test]
    fn create_arms_pending_and_still_acks_null() {
        // The create request registers the announced token (holding settle via
        // the Pending lifecycle) while the wire response stays spec-correct
        // `Null` — the ack is unchanged.
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        assert!(!server.sends_progress());

        let ack = server
            .on_request(
                "window/workDoneProgress/create",
                &json!({ "token": "tok-1" }),
            )
            .expect("create acks");
        assert_eq!(ack, Value::Null, "the create ack is spec-correct Null");
        assert_eq!(server.lifecycle(), ServerLifecycle::Pending(1));
        // A bare create does NOT flip the runtime "sends progress" capability —
        // that is reserved for a $/progress begin.
        assert!(
            !server.sends_progress(),
            "create alone is not a begin — capability discovery waits for begin"
        );
    }

    #[test]
    fn create_then_begin_then_end_walks_pending_busy_healthy() {
        // The full elm-shaped lifecycle: announce (Pending) → open bracket
        // (Busy) → close (Healthy). The created token holds settle across the
        // create→begin gap.
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);

        server
            .on_request(
                "window/workDoneProgress/create",
                &json!({ "token": "tok-1" }),
            )
            .expect("create acks");
        assert_eq!(server.lifecycle(), ServerLifecycle::Pending(1));

        server.on_notification(
            "$/progress",
            &json!({ "token": "tok-1", "value": { "kind": "begin", "title": "Initializing workspace" } }),
        );
        assert_eq!(
            server.lifecycle(),
            ServerLifecycle::Busy(1),
            "the begin upgrades the announced token to a live bracket"
        );
        assert!(server.sends_progress(), "begin flips the capability");

        server.on_notification(
            "$/progress",
            &json!({ "token": "tok-1", "value": { "kind": "end" } }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Healthy);
    }

    #[test]
    fn two_created_tokens_hold_pending_until_all_retire() {
        // Pending derives from the outstanding created-token count; the server
        // stays Pending until the last announced token is begun or ended.
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        for tok in ["a", "b"] {
            server
                .on_request("window/workDoneProgress/create", &json!({ "token": tok }))
                .expect("create acks");
        }
        assert_eq!(server.lifecycle(), ServerLifecycle::Pending(2));

        // End one never-begun token → still one announced, still Pending.
        server.on_notification(
            "$/progress",
            &json!({ "token": "a", "value": { "kind": "end" } }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Pending(1));

        // Begin the other → Busy, then end → Healthy.
        server.on_notification(
            "$/progress",
            &json!({ "token": "b", "value": { "kind": "begin", "title": "x" } }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Busy(1));
        server.on_notification(
            "$/progress",
            &json!({ "token": "b", "value": { "kind": "end" } }),
        );
        assert_eq!(server.lifecycle(), ServerLifecycle::Healthy);
    }

    #[test]
    fn shutdown_clears_a_pending_created_token() {
        // Server death releases the token registry the same way it clears the
        // progress tracker — a created-never-begun token does not survive it.
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        server
            .on_request(
                "window/workDoneProgress/create",
                &json!({ "token": "tok-1" }),
            )
            .expect("create acks");
        assert_eq!(server.lifecycle(), ServerLifecycle::Pending(1));

        server.on_shutdown();
        assert_eq!(server.lifecycle(), ServerLifecycle::Dead);
        assert!(
            !server
                .progress
                .lock()
                .expect("progress lock")
                .has_outstanding(),
            "on_shutdown clears the created token registry"
        );
    }

    #[test]
    fn create_on_terminal_server_acks_null_and_registers_nothing() {
        // A dead server's create still acks Null (spec-correct) but does not
        // repopulate the registry on_shutdown cleared.
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Dead);
        let ack = server
            .on_request(
                "window/workDoneProgress/create",
                &json!({ "token": "tok-1" }),
            )
            .expect("create acks");
        assert_eq!(ack, Value::Null);
        assert_eq!(server.lifecycle(), ServerLifecycle::Dead);
        assert!(
            !server
                .progress
                .lock()
                .expect("progress lock")
                .has_outstanding(),
            "a terminal server registers no token"
        );
    }

    // ── try_transition_probing_to_healthy tests ─────────────────────

    #[test]
    fn try_transition_probing_to_healthy_from_probing() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Probing);
        server.try_transition_probing_to_healthy();
        assert_eq!(server.lifecycle(), ServerLifecycle::Healthy);
    }

    #[test]
    fn try_transition_probing_to_healthy_idempotent_from_healthy() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        server.try_transition_probing_to_healthy();
        assert_eq!(server.lifecycle(), ServerLifecycle::Healthy);
    }

    #[test]
    fn try_transition_probing_to_healthy_noop_from_busy() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Busy(2));
        server.try_transition_probing_to_healthy();
        assert_eq!(server.lifecycle(), ServerLifecycle::Busy(2));
    }

    #[test]
    fn try_transition_probing_to_healthy_noop_from_initializing() {
        let server = test_server();
        server.try_transition_probing_to_healthy();
        assert_eq!(server.lifecycle(), ServerLifecycle::Initializing);
    }

    #[test]
    fn try_transition_probing_to_healthy_noop_from_terminal() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Failed);
        server.try_transition_probing_to_healthy();
        assert_eq!(server.lifecycle(), ServerLifecycle::Failed);

        server.set_lifecycle(ServerLifecycle::Dead);
        server.try_transition_probing_to_healthy();
        assert_eq!(server.lifecycle(), ServerLifecycle::Dead);
    }

    // ── resolve_configuration tests ─────────────────────────────────

    #[test]
    fn resolve_configuration_returns_settings() {
        let server = LspServer::new(
            "rust".to_string(),
            "ra".to_string(),
            Some(json!({"rust-analyzer": {"check": {"command": "clippy"}}})),
        );
        let result = server.resolve_configuration(Some("rust-analyzer"), None);
        assert_eq!(result, json!({"check": {"command": "clippy"}}));
    }

    #[test]
    fn resolve_configuration_ignores_scope_uri() {
        let server = LspServer::new(
            "rust".to_string(),
            "ra".to_string(),
            Some(json!({"rust-analyzer": {"check": {"command": "clippy"}}})),
        );
        // scopeUri is ignored — per-root instances have pre-merged flat settings
        let result = server.resolve_configuration(Some("rust-analyzer"), Some("file:///root-a"));
        assert_eq!(result, json!({"check": {"command": "clippy"}}));
    }

    // ── didChangeConfiguration registration tests ───────────────

    #[test]
    fn register_did_change_configuration() {
        let server = test_server();
        assert!(!server.wants_did_change_configuration());

        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "cfg-1",
                    "method": "workspace/didChangeConfiguration"
                }]}),
            )
            .expect("should succeed");

        assert!(server.wants_did_change_configuration());
    }

    #[test]
    fn unregister_did_change_configuration() {
        let server = test_server();

        // Register first
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "cfg-1",
                    "method": "workspace/didChangeConfiguration"
                }]}),
            )
            .expect("should succeed");
        assert!(server.wants_did_change_configuration());

        // Unregister
        server
            .on_request(
                "client/unregisterCapability",
                &json!({"unregisterations": [{
                    "id": "cfg-1",
                    "method": "workspace/didChangeConfiguration"
                }]}),
            )
            .expect("should succeed");
        assert!(!server.wants_did_change_configuration());
    }

    #[test]
    fn unregister_did_change_configuration_selective() {
        let server = test_server();

        // Register two IDs
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [
                    {"id": "cfg-1", "method": "workspace/didChangeConfiguration"},
                    {"id": "cfg-2", "method": "workspace/didChangeConfiguration"}
                ]}),
            )
            .expect("should succeed");
        assert!(server.wants_did_change_configuration());

        // Unregister only one
        server
            .on_request(
                "client/unregisterCapability",
                &json!({"unregisterations": [{
                    "id": "cfg-1",
                    "method": "workspace/didChangeConfiguration"
                }]}),
            )
            .expect("should succeed");
        // Still registered via cfg-2
        assert!(server.wants_did_change_configuration());

        // Unregister the second
        server
            .on_request(
                "client/unregisterCapability",
                &json!({"unregisterations": [{
                    "id": "cfg-2",
                    "method": "workspace/didChangeConfiguration"
                }]}),
            )
            .expect("should succeed");
        assert!(!server.wants_did_change_configuration());
    }

    #[test]
    fn on_shutdown_clears_config_registration() {
        let server = test_server();
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "cfg-1",
                    "method": "workspace/didChangeConfiguration"
                }]}),
            )
            .expect("should succeed");
        assert!(server.wants_did_change_configuration());

        server.on_shutdown();
        assert!(!server.wants_did_change_configuration());
    }

    #[test]
    fn ws31_review_d_on_shutdown_clears_watched_files() {
        // lsp-4 guard: shutdown must clear the watched-files registrations so a
        // dead server is never consulted for change routing. Green guard — the
        // clear landed in C2; this test makes it load-bearing.
        let server = test_server();
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "watch-1",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {"watchers": [{"globPattern": "**/*.rs"}]}
                }]}),
            )
            .expect("should succeed");
        assert_eq!(server.watched_files_snapshot().len(), 1);

        server.on_shutdown();
        assert!(
            server.watched_files_snapshot().is_empty(),
            "on_shutdown must clear watched-files registrations"
        );
    }

    // ── didChangeWatchedFiles registration tests ────────────────

    #[test]
    fn register_watched_files_stores_watchers() {
        let server = test_server();
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "watch-1",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {"watchers": [
                        {"globPattern": "**/*.rs"},
                        {"globPattern": {
                            "baseUri": "file:///project",
                            "pattern": "**/*.toml"
                        }}
                    ]}
                }]}),
            )
            .expect("should succeed");

        let snapshot = server.watched_files_snapshot();
        assert_eq!(snapshot.len(), 2);

        // The plain `**/*.rs` watcher matches a root-relative `.rs` path.
        let rs_rel = std::path::Path::new("src/lib.rs");
        let rs_abs = std::path::Path::new("/root/src/lib.rs");
        assert!(
            snapshot
                .iter()
                .any(|w| w.covers(rs_rel, rs_abs, ChangeKind::Changed)),
            "plain **/*.rs watcher should cover src/lib.rs"
        );

        // The relative `**/*.toml` watcher (baseUri file:///project) matches a
        // `.toml` file under /project but not a `.rs` file.
        let toml_rel = std::path::Path::new("Cargo.toml");
        let toml_abs = std::path::Path::new("/project/Cargo.toml");
        assert!(
            snapshot
                .iter()
                .any(|w| w.covers(toml_rel, toml_abs, ChangeKind::Changed)),
            "relative **/*.toml watcher should cover /project/Cargo.toml"
        );
    }

    #[test]
    fn register_watched_files_default_kind_is_all() {
        let server = test_server();
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "watch-1",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {"watchers": [
                        {"globPattern": "**/*.rs"}
                    ]}
                }]}),
            )
            .expect("should succeed");

        let snapshot = server.watched_files_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].kind, WATCH_KIND_ALL);
        assert_eq!(snapshot[0].kind, 7);
    }

    #[test]
    fn unregister_watched_files_removes_by_id() {
        let server = test_server();
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "watch-1",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {"watchers": [
                        {"globPattern": "**/*.rs"}
                    ]}
                }]}),
            )
            .expect("should succeed");
        assert_eq!(server.watched_files_snapshot().len(), 1);

        server
            .on_request(
                "client/unregisterCapability",
                &json!({"unregisterations": [{
                    "id": "watch-1",
                    "method": "workspace/didChangeWatchedFiles"
                }]}),
            )
            .expect("should succeed");

        assert!(server.watched_files_snapshot().is_empty());
    }

    #[test]
    fn register_malformed_watcher_is_skipped() {
        let server = test_server();
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{
                    "id": "watch-1",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {"watchers": [
                        {"kind": 1},
                        {"globPattern": "**/*.rs"}
                    ]}
                }]}),
            )
            .expect("should succeed");

        // The watcher missing globPattern is skipped; the valid one survives.
        let snapshot = server.watched_files_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].covers(
            std::path::Path::new("src/main.rs"),
            std::path::Path::new("/root/src/main.rs"),
            ChangeKind::Changed
        ));
    }

    #[test]
    fn config_change_registration_still_works() {
        let server = test_server();
        assert!(!server.wants_did_change_configuration());

        // A registration carrying both methods must populate both stores.
        server
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [
                    {"id": "cfg-1", "method": "workspace/didChangeConfiguration"},
                    {
                        "id": "watch-1",
                        "method": "workspace/didChangeWatchedFiles",
                        "registerOptions": {"watchers": [{"globPattern": "**/*.rs"}]}
                    }
                ]}),
            )
            .expect("should succeed");

        assert!(server.wants_did_change_configuration());
        assert_eq!(server.watched_files_snapshot().len(), 1);
    }

    // ── Mutant audit: supports_diagnostics OR logic ──────────────

    #[test]
    fn supports_diagnostics_text_document_sync_only() {
        // textDocumentSync present, no diagnosticProvider → should still
        // support diagnostics (push via publishDiagnostics). Blessed server so the
        // enrichment-only gate does not fire.
        let server = blessed_server_with_caps(json!({ "textDocumentSync": { "openClose": true } }));
        assert!(!server.supports_pull_diagnostics());
        assert!(server.supports_diagnostics());
    }

    #[test]
    fn supports_diagnostics_pull_only() {
        // diagnosticProvider present, no textDocumentSync → supports diagnostics.
        let server = blessed_server_with_caps(json!({ "diagnosticProvider": {} }));
        assert!(server.supports_pull_diagnostics());
        assert!(server.supports_diagnostics());
    }

    #[test]
    fn supports_diagnostics_neither() {
        let server = blessed_server_with_caps(json!({}));
        assert!(!server.supports_diagnostics());
    }

    #[test]
    fn enrichment_only_server_never_supports_diagnostics() {
        // An unverified custom def (`test-server` is absent from the blessed
        // manifest) is enrichment-only, so `supports_diagnostics` is false even
        // when it advertises both push and pull — the batch sync lifecycle never
        // engages it and its publishes are never collected (diagnostics-debt 04b).
        let server = server_with_caps(json!({
            "textDocumentSync": { "openClose": true },
            "diagnosticProvider": {}
        }));
        assert!(server.is_enrichment_only(), "test-server is unverified");
        assert!(
            !server.supports_diagnostics(),
            "an enrichment-only server is never a diagnostics source",
        );
    }

    #[test]
    fn workspace_diagnostics_gated_on_nested_flag() {
        // A pull provider without `workspaceDiagnostics` supports per-file pull
        // but NOT the whole-workspace request. Blessed server so the workspace-diag
        // capability is not withheld by the enrichment-only gate.
        let per_file = blessed_server_with_caps(json!({
            "diagnosticProvider": { "workspaceDiagnostics": false }
        }));
        assert!(per_file.supports_pull_diagnostics());
        assert!(!per_file.supports_workspace_diagnostics());

        // The nested flag flips workspace support on.
        let workspace = blessed_server_with_caps(json!({
            "diagnosticProvider": { "workspaceDiagnostics": true }
        }));
        assert!(workspace.supports_workspace_diagnostics());

        // Absent provider → no workspace support.
        assert!(!blessed_server_with_caps(json!({})).supports_workspace_diagnostics());
    }

    #[test]
    fn enrichment_only_server_never_supports_workspace_diagnostics() {
        // Even if an unverified server spontaneously advertises
        // `workspaceDiagnostics`, the enrichment-only gate withholds the whole-root
        // scope — never a diagnostics source (diagnostics-debt 04b).
        let server = server_with_caps(json!({
            "diagnosticProvider": { "workspaceDiagnostics": true }
        }));
        assert!(server.is_enrichment_only());
        assert!(!server.supports_workspace_diagnostics());
    }

    #[test]
    fn diagnostic_identifier_read_from_capability() {
        let with_id = server_with_caps(json!({
            "diagnosticProvider": { "identifier": "rustc", "workspaceDiagnostics": true }
        }));
        assert_eq!(with_id.diagnostic_identifier().as_deref(), Some("rustc"));
        // No identifier advertised → None.
        let without = server_with_caps(json!({ "diagnosticProvider": {} }));
        assert!(without.diagnostic_identifier().is_none());
    }

    // ── Mutant audit: publishes_version tracking ─────────────────

    #[test]
    fn publish_diagnostics_without_version_does_not_set_flag() {
        let server = test_server();
        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": "file:///test.rs",
                "diagnostics": []
            }),
        );
        assert!(!server.publishes_version.load(Ordering::SeqCst));
        // …but even an unversioned, empty publish is push evidence.
        assert!(server.has_ever_published());
    }

    #[tokio::test]
    async fn publish_diagnostics_with_version_sets_flag_and_notifies() {
        let server = test_server();
        assert!(!server.publishes_version.load(Ordering::SeqCst));

        // Register waiter before the first versioned diagnostic
        let notified = server.capability_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        server.on_notification(
            "textDocument/publishDiagnostics",
            &json!({
                "uri": "file:///test.rs",
                "version": 1,
                "diagnostics": []
            }),
        );
        assert!(server.publishes_version.load(Ordering::SeqCst));

        // capability_notify should fire on first versioned diagnostic
        tokio::time::timeout(Duration::from_millis(100), notified)
            .await
            .expect("capability_notify should fire on first versioned diagnostic");
    }

    // ── Mutant audit: capability_notify on first progress begin ──

    #[tokio::test]
    async fn capability_notify_fires_on_first_progress_begin() {
        let server = test_server();
        let notified = server.capability_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // First begin should fire capability_notify
        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-1",
                "value": { "kind": "begin", "title": "Test", "percentage": 0 }
            }),
        );

        tokio::time::timeout(Duration::from_millis(100), notified)
            .await
            .expect("capability_notify should fire on first progress begin");
    }

    #[tokio::test]
    async fn capability_notify_does_not_fire_on_second_progress_begin() {
        let server = test_server();

        // First begin — consumes the notification
        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-1",
                "value": { "kind": "begin", "title": "First", "percentage": 0 }
            }),
        );

        // Register waiter AFTER first begin
        let notified = server.capability_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // Second begin — should NOT fire capability_notify
        server.on_notification(
            "$/progress",
            &json!({
                "token": "tok-2",
                "value": { "kind": "begin", "title": "Second", "percentage": 0 }
            }),
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(50), notified)
                .await
                .is_err(),
            "capability_notify should not fire on second begin"
        );
    }

    // ── Mutant audit: state_notify returns internal notifier ─────

    #[tokio::test]
    async fn state_notify_fires_on_lifecycle_change() {
        let server = test_server();
        let notified = server.state_notify().notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        server.set_lifecycle(ServerLifecycle::Healthy);

        tokio::time::timeout(Duration::from_millis(100), notified)
            .await
            .expect("state_notify should fire on lifecycle change");
    }

    // ── Mutant audit: delegation methods with real Connection ────

    /// Helper: creates an `LspServer` with a real `Connection` backed by
    /// mockls. Cross-platform (mockls is a Rust binary built by this crate).
    fn server_with_connection() -> Arc<LspServer> {
        let server = Arc::new(test_server());
        let logging = LoggingServer::new();
        let bin = crate::lsp::test_support::mockls_bin();
        let (conn, _stderr) = Connection::new(
            bin.to_str().expect("mockls path is UTF-8"),
            &["test"],
            std::process::Stdio::null(),
            None,
            &server,
            "test".to_string(),
            logging,
            "test-server",
            "",
        )
        .expect("mockls should spawn");
        server.set_connection(conn);
        server
    }

    #[tokio::test]
    async fn pid_returns_process_id_with_connection() {
        let server = server_with_connection();
        let pid = server.pid();
        assert!(pid.is_some(), "pid should be Some with a live connection");
        assert!(pid.expect("just checked") > 0);
    }

    #[tokio::test]
    async fn alive_flag_returns_flag_with_connection() {
        let server = server_with_connection();
        let flag = server.alive_flag();
        assert!(
            flag.is_some(),
            "alive_flag should be Some with a connection"
        );
        assert!(
            flag.expect("just checked").load(Ordering::SeqCst),
            "alive flag should be true for a live process"
        );
    }

    #[tokio::test]
    async fn sample_tree_returns_snapshot_with_connection() {
        let server = server_with_connection();
        let snapshot = server.sample_tree();
        assert!(
            snapshot.is_some(),
            "sample_tree should return Some with a live process"
        );
    }
}
