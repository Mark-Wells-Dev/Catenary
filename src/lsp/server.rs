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
use std::path::PathBuf;
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
    /// Compiled glob (absolute or base-relative pattern).
    glob: globset::GlobMatcher,
    /// Base directory for a relative pattern, else `None` (workspace-relative).
    base: Option<PathBuf>,
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
    /// and its glob. A base-relative pattern (`base` set) matches against the
    /// path relative to that base; a workspace-relative pattern matches the
    /// root-relative path, with the absolute path as a fallback (servers
    /// register both forms). For a deletion the file is already gone from disk,
    /// but the stored `rel`/`abs` paths still match the registered glob.
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
        if let Some(base) = &self.base {
            return abs
                .strip_prefix(base)
                .is_ok_and(|sub| self.glob.is_match(sub));
        }
        self.glob.is_match(rel) || self.glob.is_match(abs)
    }

    /// Parses a single `FileSystemWatcher` JSON value into a [`ParsedWatcher`].
    ///
    /// The `globPattern` is either a string (workspace-relative or absolute)
    /// or a relative-pattern object `{ baseUri, pattern }` (Catenary
    /// advertises `relativePatternSupport: true`). The optional `kind` field
    /// is an LSP `WatchKind` bitmask; absent ⇒ [`WATCH_KIND_ALL`].
    ///
    /// Returns `None` for a malformed entry (missing/invalid `globPattern`),
    /// which the caller skips with a `debug!` rather than failing the whole
    /// registration.
    fn from_value(watcher: &Value) -> Option<Self> {
        let glob_value = watcher.get("globPattern")?;
        let (pattern, base) = match glob_value {
            Value::String(s) => (s.as_str(), None),
            Value::Object(_) => {
                let pattern = glob_value.get("pattern").and_then(Value::as_str)?;
                let base = glob_value
                    .get("baseUri")
                    .and_then(uri_to_path)
                    .map(PathBuf::from);
                (pattern, base)
            }
            _ => return None,
        };

        let glob = globset::Glob::new(pattern).ok()?.compile_matcher();

        let kind = watcher
            .get("kind")
            .and_then(Value::as_u64)
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(WATCH_KIND_ALL);

        Some(Self { glob, base, kind })
    }
}

/// Extracts a filesystem path from a relative-pattern `baseUri`.
///
/// `baseUri` is either a `file://` URI string or a `WorkspaceFolder`
/// object `{ uri, name }`. Returns the decoded path, or `None` if absent
/// or not a `file://` URI.
fn uri_to_path(base_uri: &Value) -> Option<String> {
    let uri = match base_uri {
        Value::String(s) => s.as_str(),
        Value::Object(_) => base_uri.get("uri").and_then(Value::as_str)?,
        _ => return None,
    };
    uri.strip_prefix("file://").map(ToOwned::to_owned)
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
        Self {
            capabilities: OnceLock::new(),
            supports_pull_diagnostics: AtomicBool::new(false),
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
            capability_notify: Arc::new(Notify::new()),
            progress: Arc::new(Mutex::new(ProgressTracker::new())),
            progress_notify: Arc::new(Notify::new()),
            lifecycle: Arc::new(Mutex::new(ServerLifecycle::Initializing)),
            state_notify: Arc::new(Notify::new()),
            ever_busy: AtomicBool::new(false),
            publishes_version: Arc::new(AtomicBool::new(false)),
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
        self.supports_pull_diagnostics
            .store(has("diagnosticProvider"), Ordering::SeqCst);
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
    pub fn supports_diagnostics(&self) -> bool {
        self.supports_text_document_sync
            .get()
            .copied()
            .unwrap_or(false)
            || self.supports_pull_diagnostics()
    }

    /// Returns whether the server supports pull diagnostics.
    ///
    /// Initially set from the `diagnosticProvider` capability. Can be
    /// downgraded to `false` at runtime via [`Self::downgrade_pull_diagnostics`]
    /// if the server fails the actual request.
    pub fn supports_pull_diagnostics(&self) -> bool {
        self.supports_pull_diagnostics.load(Ordering::SeqCst)
    }

    /// Permanently disables pull diagnostics for this server.
    ///
    /// Called when `textDocument/diagnostic` fails on a server that
    /// advertised `diagnosticProvider`. Subsequent calls to
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
    /// Emits a `debug!()` event on every transition so the DB sink can
    /// update the `language_servers` table. On first transition to a
    /// terminal state (`Dead` or `Failed`), additionally emits a
    /// `warn!()` notification that flows through `LoggingServer` →
    /// `NotificationQueueSink` → `systemMessage`.
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
                drop(tracker);
                self.persist_progress(db_title.as_deref(), db_message.as_deref(), db_pct);

                // Update lifecycle based on progress kind
                let kind = params["value"]["kind"].as_str();
                let mut lifecycle = self
                    .lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);

                if lifecycle.is_terminal() {
                    return;
                }

                match kind {
                    Some("begin") => {
                        let first = !self.ever_busy.swap(true, Ordering::SeqCst);
                        let new_state = match *lifecycle {
                            ServerLifecycle::Busy(n) => ServerLifecycle::Busy(n + 1),
                            _ => ServerLifecycle::Busy(1),
                        };
                        *lifecycle = new_state.clone();
                        drop(lifecycle);
                        self.persist_state(&new_state);
                        if first {
                            self.capability_notify.notify_waiters();
                        }
                    }
                    Some("end") => {
                        let new_state = match *lifecycle {
                            ServerLifecycle::Busy(n) if n > 1 => ServerLifecycle::Busy(n - 1),
                            ServerLifecycle::Busy(1) => {
                                debug!("Server ready (progress completed)");
                                ServerLifecycle::Healthy
                            }
                            ref other => other.clone(),
                        };
                        *lifecycle = new_state.clone();
                        drop(lifecycle);
                        self.persist_state(&new_state);
                    }
                    _ => {
                        drop(lifecycle);
                    }
                }

                self.progress_notify.notify_waiters();
                self.state_notify.notify_waiters();
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
            // workspace/diagnostic/refresh: server asks client to re-pull
            // diagnostics for open documents. No-op — document lifecycle
            // is transient (open/settle/save/settle/retrieve/close within
            // done_editing), so there is no stale cache to invalidate.
            // Mid-batch, the settle pipeline already waits for quiescence.
            "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "workspace/diagnostic/refresh" => Ok(Value::Null),
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

        // The string glob has no base; the relative one resolves its baseUri.
        let string_watcher = snapshot
            .iter()
            .find(|w| w.base.is_none())
            .expect("string-glob watcher present");
        assert!(string_watcher.glob.is_match("src/lib.rs"));

        let relative_watcher = snapshot
            .iter()
            .find(|w| w.base.is_some())
            .expect("relative-pattern watcher present");
        assert_eq!(
            relative_watcher.base.as_deref(),
            Some(std::path::Path::new("/project"))
        );
        assert!(relative_watcher.glob.is_match("Cargo.toml"));
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
        assert!(snapshot[0].glob.is_match("src/main.rs"));
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
        // support diagnostics (push via publishDiagnostics).
        let server = server_with_caps(json!({ "textDocumentSync": { "openClose": true } }));
        assert!(!server.supports_pull_diagnostics());
        assert!(server.supports_diagnostics());
    }

    #[test]
    fn supports_diagnostics_pull_only() {
        // diagnosticProvider present, no textDocumentSync → supports diagnostics
        let server = server_with_caps(json!({ "diagnosticProvider": {} }));
        assert!(server.supports_pull_diagnostics());
        assert!(server.supports_diagnostics());
    }

    #[test]
    fn supports_diagnostics_neither() {
        let server = server_with_caps(json!({}));
        assert!(!server.supports_diagnostics());
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
