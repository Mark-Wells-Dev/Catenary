// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Daemon-owned `state.json` snapshot.
//!
//! The daemon writes its live state as an overwrite-on-change snapshot under
//! `runtime_dir()/catenary/state.json`, written atomically (temp + rename) and
//! coalesced so a torrent of `$/progress` ticks does not thrash the file. This
//! is the out-of-process state surface that replaces the mirrored
//! `language_servers` table (observability workstream 27): a reader (the TUI,
//! `catenary list`) file-watches one small snapshot rather than re-running SQL
//! over a shared mutable store.
//!
//! [`SnapshotWriter`] is cheaply shareable via `Arc`. It owns a background
//! coalescing flush task and is wired in three places:
//!
//! - as a [`Sink`] on the [`crate::logging::LoggingServer`] — `warn`/`error`
//!   events feed the bounded [`Alert`] ring,
//! - on each [`crate::lsp::LspServer`] (via `set_snapshot`) — lifecycle,
//!   progress, and server-message transitions mutate the server board,
//! - on [`crate::lsp::LspClientManager`] — spawn registers a fresh server
//!   entry.
//!
//! The snapshot carries the full [`ServerLifecycle`] variant rather than the
//! lossy `display_state` collapse, so a stuck `probing` server is visible
//! (time-in-state = `now − state_since`).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::logging::{LogEvent, Severity, Sink};
use crate::lsp::{InstanceKey, ServerLifecycle};

/// Snapshot schema version. A stale reader skips a too-new major; this is a
/// forward-compat tag, not a migration anchor — nothing is ever read back.
const SCHEMA: u32 = 1;

/// Default coalescing window for non-urgent flushes.
///
/// A change marks the snapshot dirty and the flush task waits this long before
/// writing, so a burst of `$/progress` reports collapses into one write. A
/// lifecycle transition bypasses the wait and flushes promptly.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(150);

/// Maximum alerts retained in the ring (newest-first). Errors are rare; keep
/// history.
const MAX_ALERTS: usize = 128;

/// Maximum terminal (`died_at`-stamped) server entries retained before the
/// oldest are dropped. Coordinates with ws25 dead-server accumulation; ticket
/// 01's reaping owns the firehose side.
const MAX_DEAD_SERVERS: usize = 64;

/// Immutable daemon identity, recorded once at startup.
#[derive(Debug, Clone)]
pub struct DaemonInfo {
    /// Per-invocation instance id (`daemon:<uuid>`).
    pub instance_id: String,
    /// Daemon process id.
    pub pid: u32,
    /// Catenary version string.
    pub version: String,
    /// Daemon start time (ISO 8601).
    pub started_at: String,
}

impl DaemonInfo {
    /// Builds the serialized `daemon` block, stamping `generated_at` now.
    fn to_meta(&self) -> DaemonMeta<'_> {
        DaemonMeta {
            instance_id: &self.instance_id,
            pid: self.pid,
            version: &self.version,
            started_at: &self.started_at,
            generated_at: now_iso(),
        }
    }
}

/// Serialized `daemon` block — identity plus the snapshot's generation time.
#[derive(Debug, Serialize)]
struct DaemonMeta<'a> {
    instance_id: &'a str,
    pid: u32,
    version: &'a str,
    started_at: &'a str,
    /// When this snapshot was generated (staleness / daemon-down detection).
    generated_at: String,
}

/// Live progress for a server entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Progress {
    /// Progress operation title (e.g. `Indexing`).
    pub title: String,
    /// Optional current message (e.g. the file being scanned).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional percentage (0–100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct: Option<u32>,
}

/// Most recent `window/logMessage` for a server entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LastMessage {
    /// Severity tag (`error` / `warning` / `info` / `log` / `debug`).
    pub level: String,
    /// Message text.
    pub text: String,
    /// When the message was observed (ISO 8601).
    pub at: String,
}

/// A single LSP server's board entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerEntry {
    /// Scope id `"<server>@<scope>"` — matches the JSONL file shard and
    /// round-trips into `catenary query --server …`.
    pub id: String,
    /// Language id.
    pub language: String,
    /// Server name.
    pub server: String,
    /// Scope kind (`root` / `single_file`).
    pub scope_kind: String,
    /// Scope root path (empty for scopeless variants).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope_root: String,
    /// Full lifecycle variant (`initializing` / `probing` / `healthy` /
    /// `busy` / `failed` / `dead`) — never the lossy `display_state`.
    pub state: String,
    /// When the server entered `state` (ISO 8601). Time-in-state = `now −
    /// state_since`, so a stuck `probing` is visible.
    pub state_since: String,
    /// In-flight progress count, present only while `busy`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy_count: Option<u32>,
    /// When the server was spawned (ISO 8601).
    pub started_at: String,
    /// Active progress, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// Most recent server message, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message: Option<LastMessage>,
    /// When the server reached a terminal state (ISO 8601). Retained for a
    /// bounded window, then dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub died_at: Option<String>,
}

/// Host CLI client identity for a session board entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClientInfo {
    /// Host CLI name (`claude` / `gemini` / `antigravity`), from the hook
    /// `format` field. `"unknown"` when the session was created without one.
    pub name: String,
    /// Host CLI version, when the payload carries it. The hook payloads
    /// Catenary receives do not include a version, so this is omitted in
    /// practice (kept for forward-compat with hosts that add it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A session's current activity, derived at snapshot-build time from the
/// daemon's live editing state.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// The session holds an active editing accumulator (a covered edit is
    /// pending diagnostics).
    Editing,
    /// A `catenary diagnostics` run is in flight for the session.
    Diagnostics,
    /// Neither editing nor running diagnostics.
    #[default]
    Idle,
}

/// The most recent attributable action a session took.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LastAction {
    /// Human-readable summary, e.g. `edited src/db.rs` or
    /// `diagnostics: 2 errors, 1 warnings`.
    pub summary: String,
    /// When the action occurred (ISO 8601).
    pub at: String,
}

/// A connected session's board entry — the rich session board (ticket 05).
///
/// The board lists the sessions currently in the daemon's registry. There is
/// **no authoritative death signal** from the hook side: Antigravity sends no
/// `session-end`, and Claude's fires on exit / `/clear` but a session can be
/// resumed (which just re-creates the entry via `get_or_create_router`). So
/// `session-end` removal is best-effort, not a tombstone, and no "live-only"
/// guarantee is claimed. [`Self::last_seen`] is the liveness signal: a cold
/// session's `last_seen` freezes while `daemon.generated_at` keeps advancing
/// (driven by other sessions / server events), which is what a reaper
/// (ticket 01) or the TUI (ticket 06) keys on to judge staleness.
///
/// No `pid`: hooks own session identity (ws23) and the hook payloads carry no
/// agent pid; recovering one would mean correlating to the MCP connection,
/// which the stateless model deliberately avoids.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionEntry {
    /// Session scope id (host `session_id`) → `query --session …`; yankable.
    pub id: String,
    /// Host CLI client identity.
    pub client: ClientInfo,
    /// When the session first connected (ISO 8601).
    pub started_at: String,
    /// When the daemon last saw a hook dispatch from this session (ISO 8601).
    ///
    /// Bumped on **every** `get_or_create_router` call — i.e. every
    /// non-catenary tool the `PreToolUse` hook forwards (`Read`, `Edit`,
    /// `Bash`, …) — so it advances far more often than `last_action`, which
    /// only moves on edit / diagnostics / sed. It is the recency / liveness
    /// signal the board has no death event for (ticket 05a).
    pub last_seen: String,
    /// The session's workspace roots, taken from its own hook payload
    /// (`cwd` / `workspacePaths`) — not correlated to MCP roots.
    pub roots: Vec<String>,
    /// Current activity (editing | diagnostics | idle).
    pub status: SessionStatus,
    /// The most recent attributable action, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_action: Option<LastAction>,
}

/// Source of the live session board, pulled at each snapshot flush.
///
/// The daemon's [`SessionManager`](crate::router) owns the live session map;
/// it implements this so the [`SnapshotWriter`] can serialize the rich board
/// without holding a reference to the manager's internals. Pulled (not pushed)
/// so `status` always reflects the editing state at write time, with no
/// transition-tracking to keep in sync.
pub trait SessionBoard: Send + Sync {
    /// Builds the current session board. Called outside the snapshot lock.
    fn sessions(&self) -> Vec<SessionEntry>;
}

/// A bounded `warn`/`error` alert — "when to look".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Alert {
    /// When the alert fired (ISO 8601).
    pub at: String,
    /// Severity (`error` / `warn`).
    pub level: String,
    /// Emitting subsystem (`source` field), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Alert text.
    pub text: String,
    /// Associated scope (`<server>@<root>` or `<server>`), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Reader-side parse of a `state.json` snapshot — the daemon→TUI contract.
///
/// The owned counterpart to the writer's borrowed [`SnapshotView`]. Lives here,
/// beside the writer, so the contract is single-sourced (a round-trip test
/// keeps the two halves honest). Deserialization is **permissive**: every field
/// defaults via `#[serde(default)]`, so a missing or newly-added key never fails
/// the parse — the `schema` tag is a forward-compat hint, not a migration
/// anchor (nothing is ever read back into the writer).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// Forward-compat schema tag (see [`SCHEMA`]).
    pub schema: u32,
    /// Daemon identity + generation time.
    pub daemon: DaemonSnapshot,
    /// Server health board.
    pub servers: Vec<ServerEntry>,
    /// Session board.
    pub sessions: Vec<SessionEntry>,
    /// Bounded `warn`/`error` alert ring (newest-first).
    pub alerts: Vec<Alert>,
}

/// Reader-side `daemon` block — owned counterpart to [`DaemonMeta`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DaemonSnapshot {
    /// Per-invocation instance id (`daemon:<uuid>`).
    pub instance_id: String,
    /// Daemon process id.
    pub pid: u32,
    /// Catenary version string.
    pub version: String,
    /// Daemon start time (ISO 8601).
    pub started_at: String,
    /// When the snapshot was generated (ISO 8601) — staleness / daemon-down
    /// detection.
    pub generated_at: String,
}

/// Current UTC time as an ISO 8601 string with millisecond precision.
///
/// The single timestamp formatter for the snapshot, so every field
/// (`state_since`, `died_at`, `generated_at`, server `started_at`, alert `at`)
/// shares one representation. Callers wiring servers into the board use it so
/// the contract is uniform.
#[must_use]
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Scope id for a server instance: `"<server>@<root-or-kind>"`.
fn server_id(key: &InstanceKey) -> String {
    key.scope.root_path().map_or_else(
        || format!("{}@{}", key.server, key.scope.kind_str()),
        |root| format!("{}@{}", key.server, root.display()),
    )
}

/// The mutable in-memory state behind the snapshot.
///
/// Sessions are *not* stored here — they are pulled from the [`SessionBoard`]
/// at flush time (see [`Inner::flush`]) so `status` always reflects the live
/// editing state.
struct SnapshotState {
    daemon: DaemonInfo,
    servers: HashMap<String, ServerEntry>,
    /// Alert ring, newest-first.
    alerts: VecDeque<Alert>,
    dirty: bool,
    urgent: bool,
}

impl SnapshotState {
    /// Registers a freshly spawned server, resetting any prior entry at the
    /// same scope id (a respawn clears `died_at`, progress, and messages).
    fn register_server(&mut self, key: &InstanceKey, started_at: &str) {
        let id = server_id(key);
        let entry = ServerEntry {
            id: id.clone(),
            language: key.language_id.clone(),
            server: key.server.clone(),
            scope_kind: key.scope.kind_str().to_string(),
            scope_root: key
                .scope
                .root_path()
                .map_or_else(String::new, |p| p.display().to_string()),
            state: ServerLifecycle::Initializing.lifecycle_str().to_string(),
            state_since: now_iso(),
            busy_count: None,
            started_at: started_at.to_string(),
            progress: None,
            last_message: None,
            died_at: None,
        };
        self.servers.insert(id, entry);
    }

    /// Returns the entry for `key`, creating a fresh `initializing` one if a
    /// mutation arrives before [`Self::register_server`].
    fn ensure_entry(&mut self, key: &InstanceKey) -> &mut ServerEntry {
        let id = server_id(key);
        self.servers.entry(id.clone()).or_insert_with(|| {
            let now = now_iso();
            ServerEntry {
                id,
                language: key.language_id.clone(),
                server: key.server.clone(),
                scope_kind: key.scope.kind_str().to_string(),
                scope_root: key
                    .scope
                    .root_path()
                    .map_or_else(String::new, |p| p.display().to_string()),
                state: ServerLifecycle::Initializing.lifecycle_str().to_string(),
                state_since: now.clone(),
                busy_count: None,
                started_at: now,
                progress: None,
                last_message: None,
                died_at: None,
            }
        })
    }

    /// Applies a lifecycle transition. Resets `state_since` only when the
    /// variant changes (`Busy(n)` count changes do not reset it); stamps
    /// `died_at` on first terminal entry. Returns whether the variant changed.
    fn update_state(&mut self, key: &InstanceKey, lifecycle: &ServerLifecycle) -> bool {
        let new_state = lifecycle.lifecycle_str().to_string();
        let busy_count = match lifecycle {
            ServerLifecycle::Busy(n) => Some(*n),
            _ => None,
        };
        let terminal = lifecycle.is_terminal();
        let now = now_iso();

        let entry = self.ensure_entry(key);
        let transitioned = entry.state != new_state;
        if transitioned {
            entry.state = new_state;
            entry.state_since.clone_from(&now);
        }
        entry.busy_count = busy_count;
        if terminal && entry.died_at.is_none() {
            entry.died_at = Some(now);
        }

        if terminal {
            self.reap_dead();
        }
        transitioned
    }

    /// Replaces a server's active progress (`None` title clears it).
    fn update_progress(
        &mut self,
        key: &InstanceKey,
        title: Option<&str>,
        message: Option<&str>,
        pct: Option<u32>,
    ) {
        let entry = self.ensure_entry(key);
        entry.progress = title.map(|t| Progress {
            title: t.to_string(),
            message: message.map(str::to_string),
            pct,
        });
    }

    /// Records a server's most recent message.
    fn update_message(&mut self, key: &InstanceKey, level: &str, text: &str) {
        let entry = self.ensure_entry(key);
        entry.last_message = Some(LastMessage {
            level: level.to_string(),
            text: text.to_string(),
            at: now_iso(),
        });
    }

    /// Pushes an alert onto the newest-first ring, dropping the oldest past
    /// [`MAX_ALERTS`].
    fn push_alert(&mut self, alert: Alert) {
        self.alerts.push_front(alert);
        while self.alerts.len() > MAX_ALERTS {
            self.alerts.pop_back();
        }
    }

    /// Drops the oldest terminal entries past [`MAX_DEAD_SERVERS`].
    fn reap_dead(&mut self) {
        let mut dead: Vec<(String, String)> = self
            .servers
            .iter()
            .filter_map(|(id, e)| e.died_at.clone().map(|d| (id.clone(), d)))
            .collect();
        if dead.len() <= MAX_DEAD_SERVERS {
            return;
        }
        dead.sort_by(|a, b| a.1.cmp(&b.1));
        let drop_n = dead.len() - MAX_DEAD_SERVERS;
        for (id, _) in dead.into_iter().take(drop_n) {
            self.servers.remove(&id);
        }
    }

    /// Serializes the current state to a pretty JSON string.
    ///
    /// `sessions` is pulled from the [`SessionBoard`] by the caller (outside
    /// this struct's lock) and injected here, sorted by id for stable output.
    fn to_json(&self, sessions: &[SessionEntry]) -> String {
        let mut servers: Vec<&ServerEntry> = self.servers.values().collect();
        servers.sort_by(|a, b| a.id.cmp(&b.id));
        let mut sessions: Vec<&SessionEntry> = sessions.iter().collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let view = SnapshotView {
            schema: SCHEMA,
            daemon: self.daemon.to_meta(),
            servers,
            sessions,
            alerts: self.alerts.iter().collect(),
        };
        serde_json::to_string_pretty(&view).unwrap_or_else(|e| {
            tracing::debug!(error = %e, "state.json serialization failed");
            String::from("{}")
        })
    }
}

/// Borrowed serialization view of the snapshot.
#[derive(Serialize)]
struct SnapshotView<'a> {
    schema: u32,
    daemon: DaemonMeta<'a>,
    servers: Vec<&'a ServerEntry>,
    sessions: Vec<&'a SessionEntry>,
    alerts: Vec<&'a Alert>,
}

/// Shared inner state plus flush coordination.
struct Inner {
    state: Mutex<SnapshotState>,
    notify: Notify,
    path: PathBuf,
    coalesce: Duration,
    flush_count: AtomicU64,
    /// Live session source, wired once after the daemon's `SessionManager`
    /// exists. Pulled at flush time; absent until set (initial snapshots and
    /// transport-only tests serialize an empty session board).
    session_board: OnceLock<Arc<dyn SessionBoard>>,
}

impl Inner {
    fn lock_state(&self) -> MutexGuard<'_, SnapshotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Pulls the current session board, or an empty list when none is wired.
    ///
    /// Called *outside* the snapshot lock — the board acquires the
    /// `SessionManager`'s own locks, so holding the snapshot lock here would
    /// invert lock order against the `touch()` path (which takes the snapshot
    /// lock while a session lock may be held by the caller).
    fn sessions(&self) -> Vec<SessionEntry> {
        self.session_board
            .get()
            .map(|board| board.sessions())
            .unwrap_or_default()
    }

    /// Serializes then writes atomically.
    ///
    /// Clears the dirty flag under the lock, pulls the live session board with
    /// the lock released, then re-takes the lock only to serialize.
    fn flush(&self) {
        {
            let mut state = self.lock_state();
            if !state.dirty {
                return;
            }
            state.dirty = false;
            state.urgent = false;
        }
        // Pull sessions with the snapshot lock released (avoids lock-order
        // inversion with the SessionManager locks the board acquires).
        let sessions = self.sessions();
        let json = {
            let state = self.lock_state();
            state.to_json(&sessions)
        };
        match write_atomic(&self.path, &json) {
            Ok(()) => {
                self.flush_count.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => tracing::debug!(error = %e, "state.json write failed"),
        }
    }
}

/// Daemon-owned `state.json` writer: atomic, coalesced, background-flushed.
pub struct SnapshotWriter {
    inner: Arc<Inner>,
}

impl SnapshotWriter {
    /// Constructs a writer rooted at `dir` (the `state.json` parent) and spawns
    /// the background flush task on `runtime`, using the default coalescing
    /// window.
    #[must_use]
    pub fn new(runtime: &Handle, dir: &Path, daemon: DaemonInfo) -> Arc<Self> {
        let writer = Self::with_coalesce(runtime, dir, daemon, COALESCE_WINDOW);
        // Write an initial snapshot immediately (urgent) so the daemon meta and
        // liveness (`generated_at`) are on disk before any server spawns — a
        // reader can then distinguish "up but idle" from "down" (a missing
        // file). `with_coalesce` itself stays inert so tests start from zero.
        {
            let mut state = writer.inner.lock_state();
            state.dirty = true;
            state.urgent = true;
        }
        writer.inner.notify.notify_one();
        writer
    }

    /// Like [`Self::new`] with an explicit coalescing window (tests).
    #[must_use]
    pub fn with_coalesce(
        runtime: &Handle,
        dir: &Path,
        daemon: DaemonInfo,
        coalesce: Duration,
    ) -> Arc<Self> {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::debug!(error = %e, "failed to create state snapshot dir");
        }
        let inner = Arc::new(Inner {
            state: Mutex::new(SnapshotState {
                daemon,
                servers: HashMap::new(),
                alerts: VecDeque::new(),
                dirty: false,
                urgent: false,
            }),
            notify: Notify::new(),
            path: dir.join("state.json"),
            coalesce,
            flush_count: AtomicU64::new(0),
            session_board: OnceLock::new(),
        });
        let task_inner = inner.clone();
        runtime.spawn(async move { flush_loop(task_inner).await });
        Arc::new(Self { inner })
    }

    /// Registers a freshly spawned server (`initializing`).
    pub fn register_server(&self, key: &InstanceKey, started_at: &str) {
        {
            let mut state = self.inner.lock_state();
            state.register_server(key, started_at);
            state.dirty = true;
        }
        self.inner.notify.notify_one();
    }

    /// Applies a lifecycle transition, flushing promptly when the variant
    /// changes.
    pub fn update_state(&self, key: &InstanceKey, lifecycle: &ServerLifecycle) {
        {
            let mut state = self.inner.lock_state();
            let transitioned = state.update_state(key, lifecycle);
            state.dirty = true;
            state.urgent |= transitioned;
        }
        self.inner.notify.notify_one();
    }

    /// Updates a server's active progress (coalesced).
    pub fn update_progress(
        &self,
        key: &InstanceKey,
        title: Option<&str>,
        message: Option<&str>,
        pct: Option<u32>,
    ) {
        {
            let mut state = self.inner.lock_state();
            state.update_progress(key, title, message, pct);
            state.dirty = true;
        }
        self.inner.notify.notify_one();
    }

    /// Records a server's most recent message (coalesced).
    pub fn update_message(&self, key: &InstanceKey, level: &str, text: &str) {
        {
            let mut state = self.inner.lock_state();
            state.update_message(key, level, text);
            state.dirty = true;
        }
        self.inner.notify.notify_one();
    }

    /// Wires the live session source (the daemon's `SessionManager`).
    ///
    /// Called once, after the manager exists. Subsequent calls are ignored
    /// (the board is set-once). Marks the snapshot dirty so the first flush
    /// after wiring serializes any already-connected sessions.
    pub fn set_session_board(&self, board: Arc<dyn SessionBoard>) {
        if self.inner.session_board.set(board).is_ok() {
            self.touch();
        }
    }

    /// Marks the snapshot dirty and wakes the flush task (coalesced).
    ///
    /// Used by session action boundaries (`last_action` updates, status
    /// transitions) where the changed state lives in the [`SessionBoard`],
    /// not in this writer's own maps.
    pub fn touch(&self) {
        {
            let mut state = self.inner.lock_state();
            state.dirty = true;
        }
        self.inner.notify.notify_one();
    }

    /// Forces a synchronous flush (used by tests; the daemon relies on the
    /// background task).
    pub fn flush_now(&self) {
        self.inner.flush();
    }

    /// Number of completed flushes (test/diagnostic accessor).
    #[must_use]
    pub fn flush_count(&self) -> u64 {
        self.inner.flush_count.load(Ordering::Relaxed)
    }

    /// Path of the `state.json` file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
}

impl Sink for SnapshotWriter {
    fn handle(&self, event: &LogEvent<'_>) {
        if event.severity < Severity::Warn {
            return;
        }
        let level = match event.severity {
            Severity::Error => "error",
            _ => "warn",
        };
        let alert = Alert {
            at: now_iso(),
            level: level.to_string(),
            source: event.source.clone(),
            text: event.message.clone(),
            scope: alert_scope(event),
        };
        {
            let mut state = self.inner.lock_state();
            state.push_alert(alert);
            state.dirty = true;
        }
        self.inner.notify.notify_one();
    }
}

/// Builds an alert's `scope` from the event's `server`/`scope_root`.
fn alert_scope(event: &LogEvent<'_>) -> Option<String> {
    let server = event.server.as_deref()?;
    match event.scope_root.as_deref() {
        Some(root) if !root.is_empty() => Some(format!("{server}@{root}")),
        _ => Some(server.to_string()),
    }
}

/// Background task: waits for a dirty signal, coalesces non-urgent changes,
/// then flushes. Lives for the daemon runtime's lifetime.
async fn flush_loop(inner: Arc<Inner>) {
    loop {
        inner.notify.notified().await;
        let urgent = inner.lock_state().urgent;
        if !urgent {
            tokio::time::sleep(inner.coalesce).await;
        }
        inner.flush();
    }
}

/// Writes `contents` to `path` atomically via a temp file + rename, so a
/// concurrent reader never observes a torn file.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::lsp::Scope;
    use std::sync::atomic::AtomicBool;

    fn daemon_info() -> DaemonInfo {
        DaemonInfo {
            instance_id: "daemon:test".to_string(),
            pid: 4242,
            version: "2.0.0-test".to_string(),
            started_at: "2026-06-08T12:00:00Z".to_string(),
        }
    }

    fn fresh_state() -> SnapshotState {
        SnapshotState {
            daemon: daemon_info(),
            servers: HashMap::new(),
            alerts: VecDeque::new(),
            dirty: false,
            urgent: false,
        }
    }

    fn root_key(server: &str, root: &str) -> InstanceKey {
        InstanceKey::new(
            "rust".to_string(),
            server.to_string(),
            Scope::Root(PathBuf::from(root)),
        )
    }

    /// Polls `cond` up to ~5 s (200 × 25 ms), yielding between checks, so a
    /// scheduler-starved background flush does not flake the assertion. The
    /// coalesce-vs-urgent distinction the tests verify comes from the window
    /// size, not from a tight wall-clock deadline. Returns whether `cond` held.
    async fn poll_until(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        cond()
    }

    /// Reads + parses the snapshot file, or `None` if absent / not yet written.
    fn read_snapshot(writer: &SnapshotWriter) -> Option<serde_json::Value> {
        std::fs::read_to_string(writer.path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    #[test]
    fn server_id_uses_root_path() {
        let key = root_key("rust-analyzer", "/home/mark/Projects/Catenary");
        assert_eq!(
            server_id(&key),
            "rust-analyzer@/home/mark/Projects/Catenary"
        );
    }

    #[test]
    fn server_id_uses_kind_for_single_file() {
        let key = InstanceKey::new("text".to_string(), "ltex".to_string(), Scope::SingleFile);
        assert_eq!(server_id(&key), "ltex@single_file");
    }

    #[test]
    fn probing_server_serializes_full_state() {
        let mut state = fresh_state();
        let key = root_key("rust-analyzer", "/p");
        state.register_server(&key, "2026-06-08T12:03:44Z");
        state.update_state(&key, &ServerLifecycle::Probing);

        let json: serde_json::Value =
            serde_json::from_str(&state.to_json(&[])).expect("valid json");
        let server = &json["servers"][0];
        // Full lifecycle — NOT the lossy display_state ("initializing").
        assert_eq!(server["state"], "probing");
        assert!(server["state_since"].is_string());
        assert_eq!(server["id"], "rust-analyzer@/p");
        assert_eq!(json["schema"], 1);
        assert_eq!(json["daemon"]["pid"], 4242);
        assert!(json["daemon"]["generated_at"].is_string());
    }

    #[test]
    fn transition_resets_state_since_busy_count_does_not() {
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");

        state.update_state(&key, &ServerLifecycle::Healthy);
        let since_healthy = state.servers[&server_id(&key)].state_since.clone();

        // Guarantee a distinct millisecond so the reset is observable
        // (state_since has millisecond precision).
        std::thread::sleep(Duration::from_millis(2));

        // Healthy -> Busy(1): variant changes, state_since resets.
        let changed = state.update_state(&key, &ServerLifecycle::Busy(1));
        assert!(changed);
        let entry = &state.servers[&server_id(&key)];
        assert_eq!(entry.state, "busy");
        assert_eq!(entry.busy_count, Some(1));
        let since_busy = entry.state_since.clone();

        // Busy(1) -> Busy(2): same variant, state_since unchanged, count updates.
        let changed = state.update_state(&key, &ServerLifecycle::Busy(2));
        assert!(!changed);
        let entry = &state.servers[&server_id(&key)];
        assert_eq!(entry.busy_count, Some(2));
        assert_eq!(entry.state_since, since_busy);

        assert_ne!(since_healthy, since_busy);
    }

    #[test]
    fn dead_server_gets_died_at_then_dropped() {
        let mut state = fresh_state();

        // Stamp died_at on a single dead server.
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");
        state.update_state(&key, &ServerLifecycle::Dead);
        assert!(state.servers[&server_id(&key)].died_at.is_some());

        // Overflow the dead-server retention; oldest died_at dropped first.
        for i in 0..=MAX_DEAD_SERVERS {
            let id = format!("dead-{i}@/p");
            state.servers.insert(
                id.clone(),
                ServerEntry {
                    id,
                    language: "rust".to_string(),
                    server: format!("dead-{i}"),
                    scope_kind: "root".to_string(),
                    scope_root: "/p".to_string(),
                    state: "dead".to_string(),
                    state_since: format!("2026-06-08T12:00:{i:02}Z"),
                    busy_count: None,
                    started_at: "t0".to_string(),
                    progress: None,
                    last_message: None,
                    // Strictly increasing so the reaper's ordering is deterministic.
                    died_at: Some(format!("2026-06-08T13:00:{i:02}Z")),
                },
            );
        }
        state.reap_dead();
        let dead_count = state
            .servers
            .values()
            .filter(|e| e.died_at.is_some())
            .count();
        assert_eq!(dead_count, MAX_DEAD_SERVERS);
        // The very first dead server (oldest died_at) is gone.
        assert!(!state.servers.contains_key("dead-0@/p"));
    }

    #[test]
    fn alerts_ring_is_bounded_and_newest_first() {
        let mut state = fresh_state();
        for i in 0..(MAX_ALERTS + 10) {
            state.push_alert(Alert {
                at: format!("2026-06-08T12:00:{i:04}Z"),
                level: "warn".to_string(),
                source: None,
                text: format!("alert {i}"),
                scope: None,
            });
        }
        assert_eq!(state.alerts.len(), MAX_ALERTS);
        // Newest-first: the most recently pushed is at the front.
        assert_eq!(
            state.alerts.front().expect("non-empty").text,
            format!("alert {}", MAX_ALERTS + 9)
        );
    }

    #[test]
    fn progress_set_and_cleared() {
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.update_progress(&key, Some("Indexing"), Some("src/db.rs"), Some(62));
        let entry = &state.servers[&server_id(&key)];
        assert_eq!(
            entry.progress,
            Some(Progress {
                title: "Indexing".to_string(),
                message: Some("src/db.rs".to_string()),
                pct: Some(62),
            })
        );
        // A None title (progress ended) clears it.
        state.update_progress(&key, None, None, None);
        assert!(state.servers[&server_id(&key)].progress.is_none());
    }

    #[test]
    fn respawn_clears_died_at() {
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");
        state.update_state(&key, &ServerLifecycle::Dead);
        assert!(state.servers[&server_id(&key)].died_at.is_some());

        // A respawn at the same scope id resets the entry.
        state.register_server(&key, "t1");
        let entry = &state.servers[&server_id(&key)];
        assert!(entry.died_at.is_none());
        assert_eq!(entry.state, "initializing");
        assert_eq!(entry.started_at, "t1");
    }

    #[test]
    fn alert_scope_built_from_server_and_root() {
        let mut event = make_event(Severity::Error, "boom");
        event.server = Some("rust-analyzer".to_string());
        event.scope_root = Some("/p/Catenary".to_string());
        assert_eq!(
            alert_scope(&event),
            Some("rust-analyzer@/p/Catenary".to_string())
        );

        event.scope_root = None;
        assert_eq!(alert_scope(&event), Some("rust-analyzer".to_string()));

        event.server = None;
        assert_eq!(alert_scope(&event), None);
    }

    fn make_event(severity: Severity, message: &str) -> LogEvent<'static> {
        LogEvent {
            severity,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: None,
            client: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            scope_root: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn atomic_write_is_never_torn_under_concurrent_reader() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let stop = Arc::new(AtomicBool::new(false));

        let writer_path = path.clone();
        let writer_stop = stop.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..400 {
                // Alternate small/large content so a torn read would fail to parse.
                let body = if i % 2 == 0 {
                    "x".repeat(4000)
                } else {
                    "y".to_string()
                };
                let content = format!("{{\"i\":{i},\"body\":\"{body}\"}}");
                write_atomic(&writer_path, &content).expect("atomic write");
            }
            writer_stop.store(true, Ordering::SeqCst);
        });

        while !stop.load(Ordering::SeqCst) {
            if let Ok(s) = std::fs::read_to_string(&path) {
                let _: serde_json::Value =
                    serde_json::from_str(&s).expect("reader saw a torn (unparseable) file");
            }
        }
        writer.join().expect("writer thread");
    }

    #[tokio::test]
    async fn rapid_progress_reports_coalesce() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::with_coalesce(
            &Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_millis(50),
        );
        let key = root_key("ra", "/p");

        // Fire many progress reports faster than the coalesce window.
        let n = 40;
        for i in 0..n {
            writer.update_progress(&key, Some("Indexing"), None, Some(i));
        }

        // Poll until the final state lands on disk (robust to scheduler load).
        let settled = poll_until(|| {
            read_snapshot(&writer)
                .is_some_and(|j| j["servers"][0]["progress"]["pct"] == i64::from(n - 1))
        })
        .await;
        assert!(settled, "final progress state should reach disk");

        // Coalescing: far fewer flushes than reports. CPU load only makes the
        // task coalesce *more*, never less, so this bound cannot flake upward.
        let flushes = writer.flush_count();
        assert!(
            flushes < n.into(),
            "expected far fewer flushes than {n} reports, got {flushes}"
        );
    }

    #[tokio::test]
    async fn lifecycle_transition_flushes_promptly() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A long coalesce window: only the urgent (transition) path can flush
        // within the test's wait.
        let writer = SnapshotWriter::with_coalesce(
            &Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_secs(30),
        );
        let key = root_key("ra", "/p");
        writer.update_state(&key, &ServerLifecycle::Probing);

        // With a 30 s coalesce window, any flush within the poll deadline must
        // be the urgent (transition) path — a non-urgent flush would not fire
        // for 30 s. So a prompt flush proves urgency without a tight deadline.
        let flushed = poll_until(|| writer.flush_count() >= 1).await;
        assert!(
            flushed,
            "lifecycle transition should flush promptly (urgent)"
        );
        let json = read_snapshot(&writer).expect("state.json written");
        assert_eq!(json["servers"][0]["state"], "probing");
    }

    #[tokio::test]
    async fn alert_sink_records_warn_and_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::with_coalesce(
            &Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_millis(20),
        );

        writer.handle(&make_event(Severity::Info, "ignored"));
        writer.handle(&make_event(Severity::Warn, "a warning"));
        writer.handle(&make_event(Severity::Error, "an error"));

        let ready = poll_until(|| {
            read_snapshot(&writer)
                .is_some_and(|j| j["alerts"].as_array().is_some_and(|a| a.len() == 2))
        })
        .await;
        assert!(
            ready,
            "warn + error alerts should reach disk (info dropped)"
        );

        let json = read_snapshot(&writer).expect("state.json written");
        let alerts = json["alerts"].as_array().expect("alerts array");
        assert_eq!(alerts.len(), 2, "info dropped; warn + error kept");
        // Newest-first.
        assert_eq!(alerts[0]["level"], "error");
        assert_eq!(alerts[0]["text"], "an error");
        assert_eq!(alerts[1]["level"], "warn");
    }

    #[tokio::test]
    async fn initial_snapshot_written_on_construction() {
        // `new` (the daemon path) writes a snapshot immediately so the daemon
        // meta + liveness are visible before any server spawns.
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::new(&Handle::current(), dir.path(), daemon_info());

        let ready = poll_until(|| writer.path().exists()).await;
        assert!(ready, "state.json should be written on construction");

        let json = read_snapshot(&writer).expect("state.json written");
        assert_eq!(json["daemon"]["pid"], 4242);
        assert!(json["daemon"]["generated_at"].is_string());
        assert_eq!(
            json["servers"].as_array().expect("servers array").len(),
            0,
            "no servers spawned yet"
        );
    }

    // ── Session board (ticket 05) ──────────────────────────────────────

    /// A `SessionBoard` backed by a mutable vec, so a test can change the
    /// live board between flushes (e.g. simulate a disconnect).
    struct MockBoard(Arc<Mutex<Vec<SessionEntry>>>);

    impl SessionBoard for MockBoard {
        fn sessions(&self) -> Vec<SessionEntry> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    fn session_entry(id: &str, status: SessionStatus, roots: Vec<&str>) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            client: ClientInfo {
                name: "claude".to_string(),
                version: None,
            },
            started_at: "2026-06-08T13:10:00.000Z".to_string(),
            last_seen: "2026-06-08T13:10:00.000Z".to_string(),
            roots: roots.into_iter().map(String::from).collect(),
            status,
            last_action: None,
        }
    }

    #[test]
    fn session_entry_serializes_status_lowercase_and_omits_unknowns() {
        let mut entry = session_entry("s1", SessionStatus::Editing, vec!["/p/Catenary"]);
        entry.last_seen = "2026-06-08T13:12:00.000Z".to_string();
        entry.last_action = Some(LastAction {
            summary: "edited src/db.rs".to_string(),
            at: "2026-06-08T13:11:00.000Z".to_string(),
        });
        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["id"], "s1");
        assert_eq!(json["status"], "editing");
        assert_eq!(json["client"]["name"], "claude");
        // `last_seen` (recency) serializes as an ISO string, distinct from
        // `last_action.at` (last meaningful action) — ticket 05a.
        assert_eq!(json["last_seen"], "2026-06-08T13:12:00.000Z");
        assert!(json["last_seen"].is_string());
        // No pid field (hooks own identity; no MCP correlation). client.version
        // absent (not carried by the hook payload).
        assert!(json.get("pid").is_none(), "no pid field");
        assert!(
            json["client"].get("version").is_none(),
            "version omitted when unknown"
        );
        assert_eq!(json["roots"][0], "/p/Catenary");
        assert_eq!(json["last_action"]["summary"], "edited src/db.rs");
        assert_eq!(json["last_action"]["at"], "2026-06-08T13:11:00.000Z");
    }

    #[tokio::test]
    async fn session_board_serializes_into_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::with_coalesce(
            &Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_millis(20),
        );

        let mut editing = session_entry("sess-A", SessionStatus::Editing, vec!["/p/Catenary"]);
        editing.last_action = Some(LastAction {
            summary: "diagnostics: 2 errors, 1 warnings".to_string(),
            at: "2026-06-08T13:11:00.000Z".to_string(),
        });
        let board = Arc::new(Mutex::new(vec![editing]));
        writer.set_session_board(Arc::new(MockBoard(board.clone())));

        let ready = poll_until(|| {
            read_snapshot(&writer).is_some_and(|j| {
                j["sessions"]
                    .as_array()
                    .is_some_and(|s| s.len() == 1 && s[0]["status"] == "editing")
            })
        })
        .await;
        assert!(ready, "session board should serialize into the snapshot");

        let json = read_snapshot(&writer).expect("state.json written");
        let session = &json["sessions"][0];
        assert_eq!(session["id"], "sess-A");
        assert_eq!(session["status"], "editing");
        assert_eq!(
            session["last_action"]["summary"],
            "diagnostics: 2 errors, 1 warnings"
        );
    }

    #[tokio::test]
    async fn multi_root_session_lists_all_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::with_coalesce(
            &Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_millis(20),
        );
        let board = Arc::new(Mutex::new(vec![session_entry(
            "multi",
            SessionStatus::Idle,
            vec!["/p/A", "/p/B"],
        )]));
        writer.set_session_board(Arc::new(MockBoard(board)));

        let ready = poll_until(|| {
            read_snapshot(&writer).is_some_and(|j| {
                j["sessions"][0]["roots"]
                    .as_array()
                    .is_some_and(|r| r.len() == 2)
            })
        })
        .await;
        assert!(ready, "multi-root session should list all roots");
        let json = read_snapshot(&writer).expect("state.json written");
        let roots = json["sessions"][0]["roots"]
            .as_array()
            .expect("roots array");
        assert_eq!(roots[0], "/p/A");
        assert_eq!(roots[1], "/p/B");
    }

    #[tokio::test]
    async fn disconnected_session_vanishes_from_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::with_coalesce(
            &Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_millis(20),
        );
        let board = Arc::new(Mutex::new(vec![
            session_entry("keep", SessionStatus::Idle, vec!["/p/A"]),
            session_entry("drop", SessionStatus::Idle, vec!["/p/B"]),
        ]));
        writer.set_session_board(Arc::new(MockBoard(board.clone())));

        let two = poll_until(|| {
            read_snapshot(&writer)
                .is_some_and(|j| j["sessions"].as_array().is_some_and(|s| s.len() == 2))
        })
        .await;
        assert!(two, "both sessions should appear first");

        // Simulate a disconnect: the board now reports only one session.
        board
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|s| s.id == "keep");
        writer.touch();

        let one = poll_until(|| {
            read_snapshot(&writer).is_some_and(|j| {
                j["sessions"]
                    .as_array()
                    .is_some_and(|s| s.len() == 1 && s[0]["id"] == "keep")
            })
        })
        .await;
        assert!(one, "disconnected session should vanish on the next flush");
    }

    #[test]
    fn writer_output_parses_into_reader_snapshot() {
        // The writer serializes; the reader (`Snapshot`) must parse it back.
        // This keeps the daemon→TUI contract single-sourced: any field the
        // writer adds/renames is caught here, not in the TUI at runtime.
        let mut state = fresh_state();
        let key = root_key("rust-analyzer", "/p/Catenary");
        state.register_server(&key, "2026-06-08T12:03:44Z");
        state.update_state(&key, &ServerLifecycle::Probing);
        state.update_progress(&key, Some("Indexing"), Some("src/db.rs"), Some(62));
        state.push_alert(Alert {
            at: "2026-06-08T14:32:00.000Z".to_string(),
            level: "error".to_string(),
            source: Some("lsp".to_string()),
            text: "rust-analyzer exited".to_string(),
            scope: Some("rust-analyzer@/p/Catenary".to_string()),
        });

        let session = session_entry("mcp:7f3a", SessionStatus::Editing, vec!["/p/Catenary"]);
        let json = state.to_json(std::slice::from_ref(&session));

        let snapshot: Snapshot = serde_json::from_str(&json).expect("reader parses writer output");
        assert_eq!(snapshot.schema, SCHEMA);
        assert_eq!(snapshot.daemon.pid, 4242);
        assert!(!snapshot.daemon.generated_at.is_empty());

        assert_eq!(snapshot.servers.len(), 1);
        let server = &snapshot.servers[0];
        assert_eq!(server.id, "rust-analyzer@/p/Catenary");
        assert_eq!(server.state, "probing");
        assert!(!server.state_since.is_empty());
        assert_eq!(
            server.progress.as_ref().and_then(|p| p.pct),
            Some(62),
            "progress round-trips"
        );

        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].id, "mcp:7f3a");
        assert_eq!(snapshot.sessions[0].status, SessionStatus::Editing);

        assert_eq!(snapshot.alerts.len(), 1);
        assert_eq!(snapshot.alerts[0].level, "error");
        assert_eq!(
            snapshot.alerts[0].scope.as_deref(),
            Some("rust-analyzer@/p/Catenary")
        );
    }

    #[test]
    fn reader_tolerates_missing_and_extra_keys() {
        // Forward/back-compat: an empty object parses to defaults; unknown keys
        // are ignored; a partial server entry fills the rest from Default.
        let empty: Snapshot = serde_json::from_str("{}").expect("empty object parses");
        assert_eq!(empty.schema, 0);
        assert!(empty.servers.is_empty());

        let partial = r#"{
            "schema": 99,
            "future_field": {"nested": true},
            "servers": [{"id": "ra@/p", "state": "healthy", "unknown": 1}]
        }"#;
        let snap: Snapshot = serde_json::from_str(partial).expect("partial parses");
        assert_eq!(snap.schema, 99);
        assert_eq!(snap.servers.len(), 1);
        assert_eq!(snap.servers[0].id, "ra@/p");
        assert_eq!(snap.servers[0].state, "healthy");
        assert!(snap.servers[0].progress.is_none());
    }
}
