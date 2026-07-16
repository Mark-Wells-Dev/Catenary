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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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
///
/// - `1` — the original server/session/root board.
/// - `2` — server entries carry `respawns`/`last_died_at` (crash-loop
///   visibility) and `degraded_since`/`degraded_reason` (decision-027 coverage
///   degradation); root entries carry the full contributor `sources` and the
///   ephemeral `idle_remaining_secs`; session entries carry their live
///   `subagents` (tui-rework snapshot enrichment). Every schema-2 addition is
///   serde-additive — an older reader defaults each new field.
/// - `3` — the daemon records `activity_languages`: which configured languages
///   tracked-session activity has touched, with the root and representative
///   file(s) that made each live (tui-rework 09). The health model gates
///   suggestion/Fatal findings on this activity ledger rather than presence, and
///   renders its provenance under the finding. Serde-additive — an older reader
///   defaults the field to an empty list. Later additive additions on the same
///   schema (tui-rework 14): [`SessionStatus::Working`] (the gate-paid status),
///   plus per-subagent `status`/`last_seen` on [`Subagent`] — each defaults on a
///   reader predating it, so no schema bump is warranted. Misc 167 adds
///   server-entry `strikes`/`benched` (the demand-revive strike ledger) the
///   same way: serde-additive, defaulting to `0`/`None` on older readers.
const SCHEMA: u32 = 3;

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

/// Maximum milestones retained in the activity ring (newest-first). A curated
/// feed, so a modest window is enough to glimpse recent daemon activity
/// (observability ticket 08).
const MAX_ACTIVITY: usize = 64;

/// Maximum `(language, root)` activity buckets tracked before new ones are
/// dropped — the activity ledger is provenance, not history (tui-rework 09).
const MAX_ACTIVITY_LANGS: usize = 64;

/// Maximum distinct touched files tracked per activity bucket. Bounds the
/// count and the memory a long-lived session can accrue; the provenance render
/// only needs a representative sample plus the tally.
const MAX_ACTIVITY_FILES: usize = 128;

/// Representative touched files serialized per activity bucket — enough for a
/// provenance line, far below the tracked-file cap.
const ACTIVITY_FILES_SHOWN: usize = 8;

/// A bridge↔daemon protocol-version mismatch the daemon observed at a hello
/// (ws41-02).
///
/// Recorded into the snapshot's daemon block the moment a connecting bridge's
/// hello disagrees with the `catenary-mcp` version the daemon links, and
/// cleared once an agreeing hello arrives. It is the persistent surface behind
/// the `catenary doctor` finding, the TUI/board finding, and the `SessionStart`
/// hook line — the one interrupt fired at observation time, then this record
/// carries the reminder until the versions agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeMismatch {
    /// The bridge's reported `catenary-mcp` version, or `None` for a
    /// pre-handshake bridge that carried no version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_version: Option<String>,
    /// The `catenary-mcp` version the daemon links.
    pub daemon_version: String,
}

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
    /// Builds the daemon identity for the running binary, stamping [`version`]
    /// from the single binary-version source ([`crate::health::skew::BINARY_VERSION`],
    /// the `git describe` string `catenary version` and the skew check read).
    ///
    /// The daemon snapshot once wrote the bare `CARGO_PKG_VERSION` while the
    /// skew check compared against the git-describe `CATENARY_VERSION`, so every
    /// non-tag build read as falsely skewed (tui-rework 09, item 1). Sourcing
    /// both from the same constant makes that class of false positive
    /// impossible.
    ///
    /// [`version`]: Self::version
    #[must_use]
    pub fn current(instance_id: String, pid: u32, started_at: String) -> Self {
        Self {
            instance_id,
            pid,
            version: crate::health::skew::BINARY_VERSION.to_string(),
            started_at,
        }
    }

    /// Builds the serialized `daemon` block, stamping `generated_at` now and
    /// folding in any observed bridge↔daemon version mismatch.
    fn to_meta<'a>(&'a self, bridge_mismatch: Option<&'a BridgeMismatch>) -> DaemonMeta<'a> {
        DaemonMeta {
            instance_id: &self.instance_id,
            pid: self.pid,
            version: &self.version,
            started_at: &self.started_at,
            generated_at: now_iso(),
            bridge_mismatch,
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
    /// The observed bridge↔daemon protocol-version mismatch, if any (ws41-02).
    #[serde(skip_serializing_if = "Option::is_none")]
    bridge_mismatch: Option<&'a BridgeMismatch>,
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
    /// Outstanding work-done-token count: the in-flight `begin` bracket count
    /// while `busy`, or the announced-but-not-started token count while
    /// `pending` (misc 200). Absent in every other state.
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
    /// How many times this scope id has been re-registered (spawned again).
    /// Carried through re-registration rather than reset, so a crash-looping
    /// server renders as a climbing count instead of a healthy young one.
    pub respawns: u32,
    /// The death timestamp carried forward from the entry's previous life
    /// (ISO 8601), preserved across respawn so a crash-loop's last death stays
    /// visible even after the entry returns to `initializing`. `None` for a
    /// server that has never died before its current life.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_died_at: Option<String>,
    /// When the diagnostics path last degraded this server's coverage
    /// (decision 027) — ISO 8601, stamped on first degradation and cleared on
    /// recovery. `None` while the server is covering normally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_since: Option<String>,
    /// Why the server's coverage is degraded, paired with [`Self::degraded_since`]
    /// and cleared together on recovery. `None` while covering normally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    /// The demand-driven revive gate's strike count (misc 167): failure
    /// observations (`+1`) minus served work (`−1`), clamped to `[0, 3]`.
    /// Carried across re-registration like `respawns`, so the board mirrors
    /// the daemon ledger rather than rebirthing at zero.
    #[serde(skip_serializing_if = "u8_is_zero")]
    pub strikes: u8,
    /// Set when the server struck out (three strikes): the terminal cause —
    /// `"never started"` (no request ever served) or `"unstable"` (served,
    /// then crashed repeatedly). `None` while demand revives are permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benched: Option<String>,
}

/// `skip_serializing_if` helper: keep a zero strike count out of the JSON so
/// pre-misc-167 snapshots and healthy entries render byte-identically.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes by reference"
)]
const fn u8_is_zero(n: &u8) -> bool {
    *n == 0
}

/// Host CLI client identity for a session board entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClientInfo {
    /// Host CLI name (`claude` / `antigravity`), from the hook
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
///
/// The distinction between [`Editing`](Self::Editing) and
/// [`Working`](Self::Working) is the editing debt gate: `editing` means the gate
/// is *armed* (a covered edit is still undiagnosed), while `working` means the
/// gate has been paid — the batch is fully diagnosed but the session still holds
/// an editing accumulator (tui-rework 14, item 1). A stale reader that predates
/// `working` degrades it to [`Unknown`](Self::Unknown), which the render layer
/// treats as quiet — never as a false `editing`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// The editing gate is armed: the session's batch holds an undelivered
    /// covered file (a covered edit is pending diagnostics).
    Editing,
    /// The editing gate is paid but the session is still editing: the batch is
    /// fully delivered (diagnosed) and an editing accumulator is still held
    /// (tui-rework 14, item 1).
    Working,
    /// A `catenary diagnostics` run is in flight for the session.
    Diagnostics,
    /// Neither editing nor running diagnostics.
    #[default]
    Idle,
    /// Forward-compat catch-all: a status a newer daemon emits that this reader
    /// does not recognize. Never produced by the writer; rendered as quiet
    /// (like [`Idle`](Self::Idle)), so an unknown future status never reads as a
    /// false `editing`.
    #[serde(other)]
    Unknown,
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

/// A live subagent running under a parent session.
///
/// Populated only for hosts that feed subagent identity (Claude Code's
/// `Subagent*` events); Antigravity and OpenCode send none, so their sessions
/// carry an empty list. Additive to schema 2 — a reader predating this field
/// simply defaults it (`#[serde(default)]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Subagent {
    /// The subagent's host-supplied agent id — yankable, the sub-row label.
    pub id: String,
    /// When the subagent started (ISO 8601).
    pub started_at: String,
    /// The subagent's derived activity, from its own per-`(session, agent)`
    /// editing batch (tui-rework 14, item 3). Additive on schema 3 — a reader
    /// predating this field defaults it to [`SessionStatus::Idle`].
    pub status: SessionStatus,
    /// When the daemon last saw a hook dispatch attributed to this subagent
    /// (ISO 8601), or empty when never resolved. Additive on schema 3 — a reader
    /// predating this field defaults it to empty and the render falls back to the
    /// subagent's start time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_seen: String,
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
    /// only moves on edit / diagnostics. It is the recency / liveness
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
    /// Live subagents running under this session (Claude Code `Subagent*`
    /// flow), started-time sorted. Empty for hosts that feed no subagent
    /// identity — the board renders subagent sub-rows only where they exist
    /// (capability-aware, no fabrication).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<Subagent>,
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

/// A tracked workspace root and its class, for the daemon-level root board.
///
/// The root board is a daemon-wide view (like the server board), distinct from
/// the per-session [`SessionEntry::roots`] (which mirror a session's own hook
/// payload). It exists so ephemeral, activity-mounted roots (ephemeral-roots
/// ticket 02) are visible on `state.json` and distinguished from pinned roots.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RootEntry {
    /// The canonical root path.
    pub path: String,
    /// `true` when the root is held only by an activity-mount contributor and
    /// will expire on idle; `false` for a pinned root (`hook` / `mcp:*` /
    /// `worktree:*`).
    pub ephemeral: bool,
    /// The contributor sources holding this root (`hook` / `mcp:*` /
    /// `worktree:*` / `ephemeral:*`), sorted — the full class list `tool/roots-ls`
    /// reports, no longer collapsed to the [`Self::ephemeral`] bool alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Seconds until an ephemeral (activity-mounted) root expires on idle, when
    /// an idle clock is tracked for it. `None` for a pinned root or a root with
    /// no ephemeral mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_remaining_secs: Option<u64>,
}

/// Source of the live root board, pulled at each snapshot flush.
///
/// The daemon's [`SessionManager`](crate::router) owns the `RootTracker`; it
/// implements this so the [`SnapshotWriter`] can serialize the current tracked
/// roots with their class. Pulled (not pushed) so the board always reflects the
/// live tracker — including ephemeral mounts that come and go between flushes.
pub trait RootBoard: Send + Sync {
    /// Builds the current root board. Called outside the snapshot lock.
    fn roots(&self) -> Vec<RootEntry>;
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

/// A curated activity-ring milestone kind — a *significant* daemon event
/// promoted out of the firehose into the bounded activity ring (observability
/// ticket 08).
///
/// Unlike [`Alert`] ("when to look"), a milestone is a neutral "what happened"
/// signal: indexing finished, a diagnostics run completed, a session connected.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneKind {
    /// A server's `$/progress` burst (indexing, build-graph, …) drained.
    IndexingDone,
    /// A `catenary diagnostics` run completed (carries the result counts).
    Diagnostics,
    /// Editing mode began — the first covered edit was accumulated.
    EditingStart,
    /// Editing mode ended with no covered files to diagnose.
    EditingDone,
    /// A server became ready (`initializing`/`probing` → `healthy`).
    ServerReady,
    /// A server entered a terminal state (`failed` / `dead`).
    ServerFailed,
    /// A session connected.
    SessionConnect,
    /// A session disconnected (best-effort; the hook side has no authoritative
    /// death signal — see [`SessionEntry`]).
    SessionDisconnect,
    /// Forward-compat catch-all: a kind a newer daemon emits that this reader
    /// does not recognize. Never produced by the writer.
    #[serde(other)]
    #[default]
    Unknown,
}

/// A single activity-ring milestone — `at`, `kind`, a one-line `summary`, and a
/// yankable `scope` pointer (the bridge into `catenary query`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Milestone {
    /// When the milestone occurred (ISO 8601).
    pub at: String,
    /// What kind of milestone this is.
    pub kind: MilestoneKind,
    /// Human-readable one-line summary (e.g. `3 errors, 12 warnings · 4 files`).
    pub summary: String,
    /// Yankable scope id — `session_id` or `<server>@<root>` — or `None` when
    /// the milestone has no natural scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// One background auto-install's current standing (lsm 05), keyed by server —
/// the daemon-lifetime record behind the doctor finding and TUI awareness.
///
/// Written by the daemon's [`crate::auto_install::AutoInstaller`] at kick
/// (`installing`), landing (`installed`), and failure (`failed`, with the
/// reason in `detail`). One entry per server: a later transition overwrites the
/// earlier one, so the record always reads the latest state. Like every
/// snapshot record it lives exactly as long as the daemon — a fresh daemon
/// starts clean and the next session start's detection retries naturally.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AutoInstallEntry {
    /// Canonical server name (the `[lsp.server.*]` key).
    pub server: String,
    /// The blessed pinned version being installed.
    pub version: String,
    /// Current standing: `installing` / `installed` / `failed`.
    pub status: String,
    /// The failure reason, present only for `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the entry last transitioned (ISO 8601).
    pub at: String,
}

/// A configured language made live by tracked-session activity, with the
/// provenance that triggered it (tui-rework 09, items 4–5).
///
/// The daemon appends to this ledger whenever a tracked session touches a file
/// of a configured language (edit / read / shell write). The health model gates
/// suggestion and Fatal findings on activity — a language present only in a
/// dormant fixture directory no one touched is quiet Info inventory, not a
/// finding — and renders `root` + `files` as the "why is this being probed?"
/// provenance under the finding.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LanguageActivity {
    /// The configured language id (e.g. `cmake`, `rust`).
    pub language: String,
    /// The tracked root under which the touch happened (canonical path).
    pub root: String,
    /// Representative touched files, root-relative, sorted and bounded — the
    /// provenance sample (`routed by <file> …`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Total distinct touched files for this `(language, root)` bucket —
    /// the `(N files)` tally, which may exceed [`Self::files`].
    pub file_count: usize,
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
    /// Daemon-level tracked-root board (pinned + ephemeral).
    pub roots: Vec<RootEntry>,
    /// Bounded `warn`/`error` alert ring (newest-first).
    pub alerts: Vec<Alert>,
    /// Bounded curated activity ring (milestones, newest-first).
    pub activity: Vec<Milestone>,
    /// Configured languages made live by tracked-session activity, with their
    /// provenance (schema 3). The health model's suggestion/Fatal gate and the
    /// finding provenance read this ledger.
    pub activity_languages: Vec<LanguageActivity>,
    /// Background auto-install standings, one per server (lsm 05).
    /// Serde-additive — a snapshot from a daemon predating the field defaults
    /// to empty.
    pub auto_installs: Vec<AutoInstallEntry>,
}

impl Snapshot {
    /// The default `state.json` path: `runtime_dir()/catenary/state.json`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        crate::paths::runtime_dir()
            .join("catenary")
            .join("state.json")
    }

    /// Read and parse the running daemon's snapshot from [`Self::default_path`],
    /// or `None` when it is missing/unparseable (daemon down).
    ///
    /// Deserialization is permissive (`#[serde(default)]`), so a snapshot from a
    /// daemon predating any given field still parses — the missing field defaults.
    #[must_use]
    pub fn read_default() -> Option<Self> {
        let contents = std::fs::read_to_string(Self::default_path()).ok()?;
        serde_json::from_str(&contents).ok()
    }
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
    /// The observed bridge↔daemon protocol-version mismatch, if any (ws41-02).
    /// Absent on agreement and on snapshots from daemons predating the field.
    #[serde(default)]
    pub bridge_mismatch: Option<BridgeMismatch>,
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
    /// Activity ring (curated milestones), newest-first.
    activity: VecDeque<Milestone>,
    /// Language-activity ledger: `(language, root)` → the distinct touched
    /// files, bounded. Serialized into [`Snapshot::activity_languages`] as the
    /// health model's suggestion/Fatal gate and provenance source (tui-rework 09).
    activity_languages: BTreeMap<(String, String), BTreeSet<String>>,
    /// Background auto-install standings keyed by server (lsm 05). One entry
    /// per server — a later transition (`installing` → `installed`/`failed`)
    /// overwrites the earlier — serialized into [`Snapshot::auto_installs`].
    auto_installs: BTreeMap<String, AutoInstallEntry>,
    /// The observed bridge↔daemon protocol-version mismatch (ws41-02), set the
    /// moment a disagreeing hello arrives and cleared once the versions agree.
    /// Serialized into the `daemon` block so `catenary doctor`, the TUI board,
    /// and the `SessionStart` hook all read one record.
    bridge_mismatch: Option<BridgeMismatch>,
    dirty: bool,
    urgent: bool,
}

impl SnapshotState {
    /// Registers a freshly spawned server, clearing the prior entry's live
    /// fields (`died_at`, progress, messages, degradation) at the same scope id.
    ///
    /// A re-registration is a **respawn**: the crash history is carried forward
    /// rather than reset — the counter climbs and the prior life's death
    /// timestamp becomes `last_died_at`, so a crash-looping server no longer
    /// rebirths as a healthy young one (the crash-loop blindspot).
    fn register_server(&mut self, key: &InstanceKey, started_at: &str) {
        let id = server_id(key);
        let (respawns, last_died_at, strikes, benched) =
            self.servers.get(&id).map_or((0, None, 0, None), |prev| {
                (
                    prev.respawns.saturating_add(1),
                    prev.died_at.clone().or_else(|| prev.last_died_at.clone()),
                    // The strike ledger survives respawn (misc 167): mirror
                    // it forward like the crash history rather than
                    // rebirthing the entry at zero.
                    prev.strikes,
                    prev.benched.clone(),
                )
            });
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
            respawns,
            last_died_at,
            degraded_since: None,
            degraded_reason: None,
            strikes,
            benched,
        };
        self.servers.insert(id, entry);
    }

    /// Removes a server's board entry by scope id — a per-root teardown reaped
    /// the instance, so the board must stop showing it (bug 72). Returns whether
    /// an entry was present.
    fn remove_server(&mut self, key: &InstanceKey) -> bool {
        self.servers.remove(&server_id(key)).is_some()
    }

    /// Marks a server's coverage degraded (decision 027). `degraded_since` is
    /// stamped once (idempotent — a repeat does not move it) and `reason`
    /// refreshes. Returns whether a matching entry existed.
    fn mark_degraded(&mut self, id: &str, reason: &str) -> bool {
        if let Some(entry) = self.servers.get_mut(id) {
            if entry.degraded_since.is_none() {
                entry.degraded_since = Some(now_iso());
            }
            entry.degraded_reason = Some(reason.to_string());
            true
        } else {
            false
        }
    }

    /// Clears a server's degradation state on recovery. Returns whether a change
    /// was made (an entry existed and carried degradation state).
    fn clear_degraded(&mut self, id: &str) -> bool {
        if let Some(entry) = self.servers.get_mut(id)
            && (entry.degraded_since.is_some() || entry.degraded_reason.is_some())
        {
            entry.degraded_since = None;
            entry.degraded_reason = None;
            return true;
        }
        false
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
                respawns: 0,
                last_died_at: None,
                degraded_since: None,
                degraded_reason: None,
                strikes: 0,
                benched: None,
            }
        })
    }

    /// Mirrors an instance's strike-ledger standing (misc 167). Returns
    /// whether anything changed.
    fn update_strikes(&mut self, key: &InstanceKey, strikes: u8, benched: Option<&str>) -> bool {
        let entry = self.ensure_entry(key);
        let changed = entry.strikes != strikes || entry.benched.as_deref() != benched;
        entry.strikes = strikes;
        entry.benched = benched.map(str::to_string);
        changed
    }

    /// Applies a lifecycle transition. Resets `state_since` only when the
    /// variant changes (`Busy(n)` count changes do not reset it); stamps
    /// `died_at` on first terminal entry. Returns whether the variant changed.
    fn update_state(&mut self, key: &InstanceKey, lifecycle: &ServerLifecycle) -> bool {
        let new_state = lifecycle.lifecycle_str().to_string();
        let busy_count = match lifecycle {
            // Both the busy `begin`-bracket count and the pending
            // announced-token count ride the same field (misc 200).
            ServerLifecycle::Busy(n) | ServerLifecycle::Pending(n) => Some(*n),
            _ => None,
        };
        let terminal = lifecycle.is_terminal();
        let now = now_iso();

        let entry = self.ensure_entry(key);
        let prev_state = entry.state.clone();
        let transitioned = entry.state != new_state;
        if transitioned {
            entry.state.clone_from(&new_state);
            entry.state_since.clone_from(&now);
        }
        entry.busy_count = busy_count;
        if terminal && entry.died_at.is_none() {
            entry.died_at = Some(now.clone());
        }

        // Promote significant lifecycle transitions to the activity ring. This
        // is the single site that mirrors server state, so it is the natural
        // place to detect readiness/failure regardless of which path drove the
        // transition (probe vs. `$/progress` drain) — ticket 08.
        if transitioned {
            if new_state == "healthy" && matches!(prev_state.as_str(), "initializing" | "probing") {
                self.push_milestone(Milestone {
                    at: now.clone(),
                    kind: MilestoneKind::ServerReady,
                    summary: format!("{} ready", key.server),
                    scope: Some(server_id(key)),
                });
            } else if terminal && !matches!(prev_state.as_str(), "failed" | "dead") {
                self.push_milestone(Milestone {
                    at: now.clone(),
                    kind: MilestoneKind::ServerFailed,
                    summary: format!("{} {new_state}", key.server),
                    scope: Some(server_id(key)),
                });
            }
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
        // A `None` title clears progress: if the entry was mid-progress, this is
        // the burst draining (the `$/progress` "end" for the last active token),
        // which is the `indexing_done` milestone (ticket 08). Capture the ended
        // title before overwriting.
        let ended_title = if title.is_none() {
            entry.progress.as_ref().map(|p| p.title.clone())
        } else {
            None
        };
        entry.progress = title.map(|t| Progress {
            title: t.to_string(),
            message: message.map(str::to_string),
            pct,
        });

        if let Some(ended) = ended_title {
            let summary = if ended.is_empty() {
                format!("{} finished indexing", key.server)
            } else {
                format!("{ended} complete")
            };
            self.push_milestone(Milestone {
                at: now_iso(),
                kind: MilestoneKind::IndexingDone,
                summary,
                scope: Some(server_id(key)),
            });
        }
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

    /// Pushes a milestone onto the newest-first activity ring, dropping the
    /// oldest past [`MAX_ACTIVITY`].
    fn push_milestone(&mut self, milestone: Milestone) {
        self.activity.push_front(milestone);
        while self.activity.len() > MAX_ACTIVITY {
            self.activity.pop_back();
        }
    }

    /// Records a tracked-session file touch into the language-activity ledger,
    /// returning whether it changed the ledger (a new bucket or a new distinct
    /// file) — a `false` return leaves the snapshot clean, so steady-state
    /// re-touches never trigger a flush.
    ///
    /// Bounded on both axes: [`MAX_ACTIVITY_LANGS`] `(language, root)` buckets,
    /// [`MAX_ACTIVITY_FILES`] distinct files per bucket — the ledger is
    /// provenance, not history.
    fn record_activity(&mut self, language: &str, root: &str, file: &str) -> bool {
        let key = (language.to_string(), root.to_string());
        if let Some(files) = self.activity_languages.get_mut(&key) {
            if files.contains(file) || files.len() >= MAX_ACTIVITY_FILES {
                return false;
            }
            files.insert(file.to_string());
            return true;
        }
        if self.activity_languages.len() >= MAX_ACTIVITY_LANGS {
            return false;
        }
        self.activity_languages
            .insert(key, BTreeSet::from([file.to_string()]));
        true
    }

    /// Drops every language-activity bucket whose root is `root`, returning
    /// whether any bucket was removed (bug 93).
    ///
    /// The provenance-source counterpart to a retired root: when a worktree is
    /// landed/removed its `(language, root)` buckets must leave the ledger, or
    /// the doctor/TUI keep rendering `routed by … in <removed root>` against a
    /// path that can no longer route anything — the ghost provenance that made a
    /// config break read as a daemon break.
    fn forget_root_activity(&mut self, root: &str) -> bool {
        let before = self.activity_languages.len();
        self.activity_languages.retain(|(_, r), _| r != root);
        before != self.activity_languages.len()
    }

    /// Materializes the language-activity ledger into serializable entries,
    /// sorted by `(language, root)` for stable output.
    fn activity_languages(&self) -> Vec<LanguageActivity> {
        self.activity_languages
            .iter()
            .map(|((language, root), files)| LanguageActivity {
                language: language.clone(),
                root: root.clone(),
                files: files.iter().take(ACTIVITY_FILES_SHOWN).cloned().collect(),
                file_count: files.len(),
            })
            .collect()
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
    fn to_json(&self, sessions: &[SessionEntry], roots: &[RootEntry]) -> String {
        let mut servers: Vec<&ServerEntry> = self.servers.values().collect();
        servers.sort_by(|a, b| a.id.cmp(&b.id));
        let mut sessions: Vec<&SessionEntry> = sessions.iter().collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let mut roots: Vec<&RootEntry> = roots.iter().collect();
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        let view = SnapshotView {
            schema: SCHEMA,
            daemon: self.daemon.to_meta(self.bridge_mismatch.as_ref()),
            servers,
            sessions,
            roots,
            alerts: self.alerts.iter().collect(),
            activity: self.activity.iter().collect(),
            activity_languages: self.activity_languages(),
            auto_installs: self.auto_installs.values().collect(),
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
    roots: Vec<&'a RootEntry>,
    alerts: Vec<&'a Alert>,
    activity: Vec<&'a Milestone>,
    activity_languages: Vec<LanguageActivity>,
    auto_installs: Vec<&'a AutoInstallEntry>,
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
    /// Live root source (the daemon's `RootTracker`), wired once alongside the
    /// session board. Pulled at flush; absent until set (empty root board).
    root_board: OnceLock<Arc<dyn RootBoard>>,
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

    /// Pulls the current root board, or an empty list when none is wired.
    ///
    /// Called *outside* the snapshot lock, for the same lock-ordering reason as
    /// [`Self::sessions`]: the board acquires the `RootTracker`'s own lock.
    fn roots(&self) -> Vec<RootEntry> {
        self.root_board
            .get()
            .map(|board| board.roots())
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
        // Pull sessions + roots with the snapshot lock released (avoids
        // lock-order inversion with the SessionManager locks the boards acquire).
        let sessions = self.sessions();
        let roots = self.roots();
        let json = {
            let state = self.lock_state();
            state.to_json(&sessions, &roots)
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
                activity: VecDeque::new(),
                activity_languages: BTreeMap::new(),
                auto_installs: BTreeMap::new(),
                bridge_mismatch: None,
                dirty: false,
                urgent: false,
            }),
            notify: Notify::new(),
            path: dir.join("state.json"),
            coalesce,
            flush_count: AtomicU64::new(0),
            session_board: OnceLock::new(),
            root_board: OnceLock::new(),
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

    /// Removes a server's board entry on per-root teardown (worktree reap,
    /// `SubagentStop` reap, idle expiry — bug 72).
    ///
    /// A reaped per-root instance holds no process, so the board must not keep
    /// rendering it healthy. Flushes promptly (urgent), like a lifecycle change:
    /// the maintainer watches the board for the server/RAM footprint, and a
    /// stale ghost misreports exactly that.
    pub fn remove_server(&self, key: &InstanceKey) {
        {
            let mut state = self.inner.lock_state();
            if state.remove_server(key) {
                state.dirty = true;
                state.urgent = true;
            }
        }
        self.inner.notify.notify_one();
    }

    /// Records — or clears — the observed bridge↔daemon protocol-version
    /// mismatch (ws41-02, coalesced flush).
    ///
    /// `bridge` is the connecting bridge's reported `catenary-mcp` version
    /// (`None` for a pre-handshake bridge that carried no version); `daemon` is
    /// the version this daemon links. On disagreement the record is set so the
    /// persistent surfaces (doctor, board, `SessionStart`) carry the reminder;
    /// on agreement it is cleared, so a `/mcp` restart or a daemon bounce that
    /// heals the pairing silences every surface. No-op when the recorded state
    /// already matches, so a healthy stream of agreeing hellos never churns the
    /// snapshot.
    pub fn record_bridge_mismatch(&self, bridge: Option<&str>, daemon: &str) {
        let desired = catenary_mcp::version_mismatch(bridge, daemon).map(|_| BridgeMismatch {
            bridge_version: bridge.map(str::to_string),
            daemon_version: daemon.to_string(),
        });
        {
            let mut state = self.inner.lock_state();
            if state.bridge_mismatch == desired {
                return;
            }
            state.bridge_mismatch = desired;
            state.dirty = true;
            state.urgent = true;
        }
        self.inner.notify.notify_one();
    }

    /// Marks a server's coverage degraded (decision 027), stamping
    /// `degraded_since` on first degradation and refreshing the reason
    /// (coalesced flush). No-op when no entry matches `id`.
    pub fn mark_degraded(&self, id: &str, reason: &str) {
        {
            let mut state = self.inner.lock_state();
            if state.mark_degraded(id, reason) {
                state.dirty = true;
            }
        }
        self.inner.notify.notify_one();
    }

    /// Mirrors an instance's strike-ledger standing (misc 167, coalesced
    /// flush): the current strike count and — once benched — the terminal
    /// cause label the health surfaces render. Creates the entry when a
    /// spawn-fail-class instance never registered.
    pub fn update_strikes(&self, key: &InstanceKey, strikes: u8, benched: Option<&str>) {
        {
            let mut state = self.inner.lock_state();
            if state.update_strikes(key, strikes, benched) {
                state.dirty = true;
            }
        }
        self.inner.notify.notify_one();
    }

    /// Clears a server's degradation state on recovery (coalesced flush).
    /// No-op when the entry is absent or was not degraded.
    pub fn clear_degraded(&self, id: &str) {
        {
            let mut state = self.inner.lock_state();
            if state.clear_degraded(id) {
                state.dirty = true;
            }
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

    /// Wires the live root source (the daemon's `RootTracker`).
    ///
    /// Called once, after the manager exists. Subsequent calls are ignored
    /// (set-once). Marks the snapshot dirty so the first flush after wiring
    /// serializes any already-tracked roots.
    pub fn set_root_board(&self, board: Arc<dyn RootBoard>) {
        if self.inner.root_board.set(board).is_ok() {
            self.touch();
        }
    }

    /// Records a curated milestone on the activity ring (coalesced flush).
    ///
    /// Server lifecycle milestones (`server_ready`, `server_failed`,
    /// `indexing_done`) are detected internally by [`Self::update_state`] /
    /// [`Self::update_progress`] — the same sites that mirror server state. This
    /// entry point is for the session / editing / diagnostics milestones whose
    /// detection lives in the router and hook layers (ticket 08).
    pub fn record_milestone(
        &self,
        kind: MilestoneKind,
        summary: impl Into<String>,
        scope: Option<String>,
    ) {
        {
            let mut state = self.inner.lock_state();
            state.push_milestone(Milestone {
                at: now_iso(),
                kind,
                summary: summary.into(),
                scope,
            });
            state.dirty = true;
        }
        self.inner.notify.notify_one();
    }

    /// Records a tracked-session file touch into the language-activity ledger
    /// (coalesced flush). No-op — leaving the snapshot clean — when the touch is
    /// already recorded or a bound is hit, so a busy session never flush-storms.
    ///
    /// `root` is the tracked root the file lives under (the provenance root);
    /// `file` is that file root-relative. The health model gates suggestion and
    /// Fatal findings on this ledger (tui-rework 09, item 5).
    pub fn record_activity(&self, language: &str, root: &str, file: &str) {
        let changed = {
            let mut state = self.inner.lock_state();
            let changed = state.record_activity(language, root, file);
            if changed {
                state.dirty = true;
            }
            changed
        };
        if changed {
            self.inner.notify.notify_one();
        }
    }

    /// Records a background auto-install standing (lsm 05) — keyed by server,
    /// so a completion/failure overwrites the `installing` record. Flushes
    /// promptly (urgent): the record is the doctor/TUI-visible half of the
    /// announcement, and a stale `installing` after a landed install misreports
    /// exactly what the operator is watching for.
    pub fn record_auto_install(
        &self,
        server: &str,
        version: &str,
        status: &str,
        detail: Option<&str>,
    ) {
        {
            let mut state = self.inner.lock_state();
            state.auto_installs.insert(
                server.to_owned(),
                AutoInstallEntry {
                    server: server.to_owned(),
                    version: version.to_owned(),
                    status: status.to_owned(),
                    detail: detail.map(str::to_owned),
                    at: now_iso(),
                },
            );
            state.dirty = true;
            state.urgent = true;
        }
        self.inner.notify.notify_one();
    }

    /// Prunes the language-activity ledger of every bucket rooted at `root`
    /// (bug 93): the provenance source a retired root must leave behind.
    ///
    /// Called when a worktree is landed/removed so the doctor and TUI stop
    /// rendering `routed by … in <removed root>` against a path that can no
    /// longer route anything. Flushes promptly (urgent), like a lifecycle
    /// change — a stale provenance ghost misdirects triage exactly as a stale
    /// server ghost does. No-op (no flush) when the root held no activity.
    pub fn forget_root(&self, root: &str) {
        let changed = {
            let mut state = self.inner.lock_state();
            let changed = state.forget_root_activity(root);
            if changed {
                state.dirty = true;
                state.urgent = true;
            }
            changed
        };
        if changed {
            self.inner.notify.notify_one();
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
            activity: VecDeque::new(),
            activity_languages: BTreeMap::new(),
            auto_installs: BTreeMap::new(),
            bridge_mismatch: None,
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
            serde_json::from_str(&state.to_json(&[], &[])).expect("valid json");
        let server = &json["servers"][0];
        // Full lifecycle — NOT the lossy display_state ("initializing").
        assert_eq!(server["state"], "probing");
        assert!(server["state_since"].is_string());
        assert_eq!(server["id"], "rust-analyzer@/p");
        assert_eq!(json["schema"], 3);
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
                    respawns: 0,
                    last_died_at: None,
                    degraded_since: None,
                    degraded_reason: None,
                    strikes: 0,
                    benched: None,
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
    fn update_strikes_mirrors_ledger_and_survives_respawn() {
        // misc 167: the board mirrors the strike ledger, carries it across
        // re-registration (like `respawns`), and reports no change on a
        // same-value write (no dirty churn).
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");

        assert!(state.update_strikes(&key, 2, None));
        assert_eq!(state.servers[&server_id(&key)].strikes, 2);
        assert!(state.servers[&server_id(&key)].benched.is_none());

        // A respawn (re-registration) carries the standing forward.
        state.register_server(&key, "t1");
        assert_eq!(state.servers[&server_id(&key)].strikes, 2);

        // The benched label lands with the cap.
        assert!(state.update_strikes(&key, 3, Some("unstable")));
        assert_eq!(
            state.servers[&server_id(&key)].benched.as_deref(),
            Some("unstable"),
        );

        // Idempotent write: nothing changed, nothing reported.
        assert!(!state.update_strikes(&key, 3, Some("unstable")));
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
    fn respawn_increments_counter_and_preserves_last_died_at() {
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");
        assert_eq!(state.servers[&server_id(&key)].respawns, 0);
        state.update_state(&key, &ServerLifecycle::Dead);
        let first_death = state.servers[&server_id(&key)]
            .died_at
            .clone()
            .expect("died_at stamped");

        // A respawn at the same scope id revives the live fields (fresh
        // `initializing`, cleared `died_at`) but carries the crash history: the
        // counter climbs and the prior death becomes `last_died_at`. A
        // crash-loop is now legible in the raw snapshot instead of rendering as
        // a healthy young server.
        state.register_server(&key, "t1");
        let entry = &state.servers[&server_id(&key)];
        assert!(entry.died_at.is_none(), "live fields reset on respawn");
        assert_eq!(entry.state, "initializing");
        assert_eq!(entry.started_at, "t1");
        assert_eq!(entry.respawns, 1, "respawn increments the counter");
        assert_eq!(
            entry.last_died_at.as_ref(),
            Some(&first_death),
            "the prior life's death is preserved"
        );

        // A second death + respawn keeps climbing and tracks the latest death.
        state.update_state(&key, &ServerLifecycle::Dead);
        let second_death = state.servers[&server_id(&key)]
            .died_at
            .clone()
            .expect("died_at stamped again");
        state.register_server(&key, "t2");
        let entry = &state.servers[&server_id(&key)];
        assert_eq!(entry.respawns, 2);
        assert_eq!(entry.last_died_at.as_ref(), Some(&second_death));
    }

    #[test]
    fn remove_server_drops_entry() {
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");
        state.update_state(&key, &ServerLifecycle::Healthy);
        assert!(state.servers.contains_key(&server_id(&key)));

        // A per-root teardown removes the entry outright (bug 72): a reaped
        // instance leaves no healthy ghost on the board.
        assert!(state.remove_server(&key), "entry was present");
        assert!(!state.servers.contains_key(&server_id(&key)));
        // Idempotent: a second removal reports nothing to remove.
        assert!(!state.remove_server(&key));
    }

    #[test]
    fn degradation_set_and_cleared_round_trips() {
        let mut state = fresh_state();
        let key = root_key("rust-analyzer", "/p");
        state.register_server(&key, "t0");
        let id = server_id(&key);
        let entry = &state.servers[&id];
        assert!(entry.degraded_since.is_none());

        // Decision 027: the diagnostics path degrades the server's coverage.
        assert!(state.mark_degraded(&id, "unavailable during diagnostics"));
        let stamped = state.servers[&id]
            .degraded_since
            .clone()
            .expect("degraded_since stamped");
        assert_eq!(
            state.servers[&id].degraded_reason.as_deref(),
            Some("unavailable during diagnostics")
        );

        // Marking again is idempotent for the timestamp (does not move it).
        assert!(state.mark_degraded(&id, "still unavailable"));
        assert_eq!(state.servers[&id].degraded_since.as_ref(), Some(&stamped));
        assert_eq!(
            state.servers[&id].degraded_reason.as_deref(),
            Some("still unavailable")
        );

        // Recovery clears both fields.
        assert!(state.clear_degraded(&id));
        assert!(state.servers[&id].degraded_since.is_none());
        assert!(state.servers[&id].degraded_reason.is_none());
        // Clearing an already-clean entry is a no-op.
        assert!(!state.clear_degraded(&id));
        // Marking an unknown id finds no entry.
        assert!(!state.mark_degraded("nope@/p", "x"));
    }

    #[test]
    fn respawn_clears_degradation() {
        // A respawn produces a fresh server: any prior degradation is stale and
        // must not carry over (the next diagnostics run re-evaluates coverage).
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");
        let id = server_id(&key);
        state.mark_degraded(&id, "unavailable during diagnostics");
        state.update_state(&key, &ServerLifecycle::Dead);
        state.register_server(&key, "t1");
        let entry = &state.servers[&id];
        assert!(entry.degraded_since.is_none());
        assert!(entry.degraded_reason.is_none());
    }

    // ── Activity ring (ticket 08) ──────────────────────────────────────

    #[test]
    fn probing_to_healthy_pushes_server_ready_milestone() {
        let mut state = fresh_state();
        let key = root_key("rust-analyzer", "/p/Catenary");
        state.register_server(&key, "t0");
        // initializing -> probing: not a milestone.
        state.update_state(&key, &ServerLifecycle::Probing);
        assert!(
            state.activity.is_empty(),
            "probing alone is not a milestone"
        );
        // probing -> healthy: server_ready.
        state.update_state(&key, &ServerLifecycle::Healthy);
        assert_eq!(state.activity.len(), 1);
        let m = state.activity.front().expect("milestone");
        assert_eq!(m.kind, MilestoneKind::ServerReady);
        assert_eq!(m.summary, "rust-analyzer ready");
        assert_eq!(m.scope.as_deref(), Some("rust-analyzer@/p/Catenary"));
    }

    #[test]
    fn busy_to_healthy_does_not_repeat_server_ready() {
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");
        state.update_state(&key, &ServerLifecycle::Probing);
        state.update_state(&key, &ServerLifecycle::Healthy);
        // The server cycles healthy -> busy -> healthy on later work; that
        // Busy -> Healthy transition must not emit another server_ready.
        state.update_state(&key, &ServerLifecycle::Busy(1));
        state.update_state(&key, &ServerLifecycle::Healthy);
        let ready = state
            .activity
            .iter()
            .filter(|m| m.kind == MilestoneKind::ServerReady)
            .count();
        assert_eq!(ready, 1, "server_ready fires only on the first readiness");
    }

    #[test]
    fn terminal_transition_pushes_server_failed_milestone() {
        let mut state = fresh_state();
        let key = root_key("ra", "/p");
        state.register_server(&key, "t0");
        state.update_state(&key, &ServerLifecycle::Healthy);
        state.update_state(&key, &ServerLifecycle::Dead);
        let m = state.activity.front().expect("milestone");
        assert_eq!(m.kind, MilestoneKind::ServerFailed);
        assert_eq!(m.summary, "ra dead");
        // A redundant terminal update does not push a second failure.
        state.update_state(&key, &ServerLifecycle::Dead);
        let failed = state
            .activity
            .iter()
            .filter(|m| m.kind == MilestoneKind::ServerFailed)
            .count();
        assert_eq!(failed, 1);
    }

    #[test]
    fn progress_drain_pushes_indexing_done_milestone() {
        let mut state = fresh_state();
        let key = root_key("rust-analyzer", "/p/Catenary");
        state.update_progress(&key, Some("Indexing"), Some("src/db.rs"), Some(62));
        assert!(
            state.activity.is_empty(),
            "active progress is not a milestone"
        );
        // Clearing progress (the last `$/progress` "end") drains the burst.
        state.update_progress(&key, None, None, None);
        let m = state.activity.front().expect("milestone");
        assert_eq!(m.kind, MilestoneKind::IndexingDone);
        assert_eq!(m.summary, "Indexing complete");
        assert_eq!(m.scope.as_deref(), Some("rust-analyzer@/p/Catenary"));
    }

    #[test]
    fn activity_ring_is_bounded_and_newest_first() {
        let mut state = fresh_state();
        for i in 0..(MAX_ACTIVITY + 10) {
            state.push_milestone(Milestone {
                at: format!("2026-06-08T12:00:{i:04}Z"),
                kind: MilestoneKind::Diagnostics,
                summary: format!("run {i}"),
                scope: None,
            });
        }
        assert_eq!(state.activity.len(), MAX_ACTIVITY);
        // Newest-first: the most recently pushed is at the front.
        assert_eq!(
            state.activity.front().expect("non-empty").summary,
            format!("run {}", MAX_ACTIVITY + 9)
        );
    }

    #[test]
    fn milestone_kind_round_trips_and_tolerates_unknown() {
        // Known kinds serialize to snake_case and parse back.
        let json = serde_json::to_string(&MilestoneKind::IndexingDone).expect("serialize");
        assert_eq!(json, "\"indexing_done\"");
        let back: MilestoneKind = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, MilestoneKind::IndexingDone);
        // A future kind this reader does not know falls back to Unknown, not a
        // parse error (forward-compat).
        let unknown: MilestoneKind =
            serde_json::from_str("\"some_future_kind\"").expect("unknown tolerated");
        assert_eq!(unknown, MilestoneKind::Unknown);
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
    async fn record_milestone_reaches_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::with_coalesce(
            &Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_millis(20),
        );

        writer.record_milestone(
            MilestoneKind::Diagnostics,
            "3 errors, 12 warnings · 4 files",
            Some("mcp:abc".to_string()),
        );

        let ready = poll_until(|| {
            read_snapshot(&writer)
                .is_some_and(|j| j["activity"].as_array().is_some_and(|a| a.len() == 1))
        })
        .await;
        assert!(ready, "milestone should reach the snapshot");

        let json = read_snapshot(&writer).expect("state.json written");
        let m = &json["activity"][0];
        assert_eq!(m["kind"], "diagnostics");
        assert_eq!(m["summary"], "3 errors, 12 warnings · 4 files");
        assert_eq!(m["scope"], "mcp:abc");
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
            subagents: Vec::new(),
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
        // No subagents → the field is omitted (skip_serializing_if empty).
        assert!(
            json.get("subagents").is_none(),
            "empty subagents omitted for hosts that feed no subagent identity",
        );
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

    #[test]
    fn session_subagents_round_trip() {
        let mut entry = session_entry("s1", SessionStatus::Idle, vec!["/p/Catenary"]);
        entry.subagents = vec![
            Subagent {
                id: "agent-a".to_string(),
                started_at: "2026-06-08T13:10:00.000Z".to_string(),
                status: SessionStatus::Editing,
                ..Subagent::default()
            },
            Subagent {
                id: "agent-b".to_string(),
                started_at: "2026-06-08T13:11:00.000Z".to_string(),
                ..Subagent::default()
            },
        ];
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: SessionEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.subagents.len(),
            2,
            "both subagents survive the round-trip"
        );
        assert_eq!(back.subagents[0].id, "agent-a");
        assert_eq!(back.subagents[1].started_at, "2026-06-08T13:11:00.000Z");
        // The per-subagent status field (tui-rework 14, item 3) round-trips.
        assert_eq!(back.subagents[0].status, SessionStatus::Editing);
        assert_eq!(back.subagents[1].status, SessionStatus::Idle);
    }

    #[test]
    fn session_without_subagents_field_defaults_empty() {
        // Schema-2 additive guarantee: a snapshot written before this field
        // still parses, defaulting `subagents` to empty.
        let entry: SessionEntry =
            serde_json::from_str(r#"{"id":"s1","started_at":"t","last_seen":"t","status":"idle"}"#)
                .expect("legacy session entry parses");
        assert!(entry.subagents.is_empty(), "missing field defaults empty");
    }

    #[test]
    fn session_status_working_serializes_and_unknown_variant_tolerated() {
        // The `working` (gate-paid) status round-trips lowercase (item 1) …
        let entry = session_entry("s1", SessionStatus::Working, vec!["/p/Catenary"]);
        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["status"], "working");
        // … and a future status a stale reader cannot name degrades to `Unknown`
        // (never a false `editing`) rather than failing the parse.
        let back: SessionEntry = serde_json::from_str(
            r#"{"id":"s1","started_at":"t","last_seen":"t","status":"future"}"#,
        )
        .expect("unknown status variant tolerated");
        assert_eq!(back.status, SessionStatus::Unknown);
    }

    #[test]
    fn subagent_status_field_defaults_on_legacy_entry() {
        // Schema-3 additive: a subagent written before `status`/`last_seen`
        // still parses, defaulting them (item 3).
        let sub: Subagent =
            serde_json::from_str(r#"{"id":"agent-a","started_at":"t"}"#).expect("legacy subagent");
        assert_eq!(sub.status, SessionStatus::Idle);
        assert!(sub.last_seen.is_empty());
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
        state.push_milestone(Milestone {
            at: "2026-06-08T14:31:00.000Z".to_string(),
            kind: MilestoneKind::Diagnostics,
            summary: "2 errors, 1 warnings · 3 files".to_string(),
            scope: Some("mcp:7f3a".to_string()),
        });
        state.record_activity("rust", "/p/Catenary", "src/db.rs");

        let session = session_entry("mcp:7f3a", SessionStatus::Editing, vec!["/p/Catenary"]);
        let roots = vec![
            RootEntry {
                path: "/p/Catenary".to_string(),
                ephemeral: false,
                sources: vec!["hook".to_string(), "mcp:3".to_string()],
                idle_remaining_secs: None,
            },
            RootEntry {
                path: "/p/Lattice".to_string(),
                ephemeral: true,
                sources: vec!["ephemeral:/p/Lattice".to_string()],
                idle_remaining_secs: Some(312),
            },
        ];
        let json = state.to_json(std::slice::from_ref(&session), &roots);

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

        // The daemon-level root board round-trips, sorted by path, with the
        // pinned/ephemeral class, the full contributor sources, and the
        // ephemeral idle-remaining figure preserved.
        assert_eq!(snapshot.roots.len(), 2);
        assert_eq!(snapshot.roots[0].path, "/p/Catenary");
        assert!(!snapshot.roots[0].ephemeral, "pinned root");
        assert_eq!(snapshot.roots[0].sources, vec!["hook", "mcp:3"]);
        assert!(snapshot.roots[0].idle_remaining_secs.is_none());
        assert_eq!(snapshot.roots[1].path, "/p/Lattice");
        assert!(snapshot.roots[1].ephemeral, "ephemeral root");
        assert_eq!(snapshot.roots[1].sources, vec!["ephemeral:/p/Lattice"]);
        assert_eq!(snapshot.roots[1].idle_remaining_secs, Some(312));

        assert_eq!(snapshot.alerts.len(), 1);
        assert_eq!(snapshot.alerts[0].level, "error");
        assert_eq!(
            snapshot.alerts[0].scope.as_deref(),
            Some("rust-analyzer@/p/Catenary")
        );

        assert_eq!(snapshot.activity.len(), 1);
        assert_eq!(snapshot.activity[0].kind, MilestoneKind::Diagnostics);
        assert_eq!(
            snapshot.activity[0].summary,
            "2 errors, 1 warnings · 3 files"
        );
        assert_eq!(snapshot.activity[0].scope.as_deref(), Some("mcp:7f3a"));

        // The language-activity ledger round-trips with its provenance.
        assert_eq!(snapshot.activity_languages.len(), 1);
        let act = &snapshot.activity_languages[0];
        assert_eq!(act.language, "rust");
        assert_eq!(act.root, "/p/Catenary");
        assert_eq!(act.files, vec!["src/db.rs"]);
        assert_eq!(act.file_count, 1);
    }

    #[test]
    fn bridge_mismatch_round_trips_through_the_snapshot() {
        // The daemon records a bridge↔daemon mismatch; the reader must parse it
        // back so `catenary doctor` and the board render the persistent finding.
        let mut state = fresh_state();
        state.bridge_mismatch = Some(BridgeMismatch {
            bridge_version: Some("2.0.1".to_string()),
            daemon_version: "2.0.2".to_string(),
        });
        let json = state.to_json(&[], &[]);
        let snapshot: Snapshot = serde_json::from_str(&json).expect("reader parses writer output");
        let recorded = snapshot
            .daemon
            .bridge_mismatch
            .expect("mismatch round-trips onto the daemon block");
        assert_eq!(recorded.bridge_version.as_deref(), Some("2.0.1"));
        assert_eq!(recorded.daemon_version, "2.0.2");

        // A pre-handshake bridge (no version) round-trips as an absent
        // bridge_version.
        state.bridge_mismatch = Some(BridgeMismatch {
            bridge_version: None,
            daemon_version: "2.0.2".to_string(),
        });
        let json = state.to_json(&[], &[]);
        let snapshot: Snapshot = serde_json::from_str(&json).expect("parse");
        let recorded = snapshot.daemon.bridge_mismatch.expect("mismatch present");
        assert!(
            recorded.bridge_version.is_none(),
            "pre-handshake bridge round-trips as None",
        );

        // Agreement clears the record — no daemon block field.
        state.bridge_mismatch = None;
        let json = state.to_json(&[], &[]);
        let snapshot: Snapshot = serde_json::from_str(&json).expect("parse");
        assert!(
            snapshot.daemon.bridge_mismatch.is_none(),
            "an agreeing pairing leaves no record — the finding self-clears",
        );
    }

    #[test]
    fn auto_install_records_round_trip_latest_state_per_server() {
        // lsm 05: one record per server — a failure overwrites the earlier
        // `installing` — and the reader parses it back for the doctor finding.
        let mut state = fresh_state();
        state.auto_installs.insert(
            "gopls".to_string(),
            AutoInstallEntry {
                server: "gopls".to_string(),
                version: "v0.20.0".to_string(),
                status: "installing".to_string(),
                detail: None,
                at: now_iso(),
            },
        );
        state.auto_installs.insert(
            "gopls".to_string(),
            AutoInstallEntry {
                server: "gopls".to_string(),
                version: "v0.20.0".to_string(),
                status: "failed".to_string(),
                detail: Some("registry unreachable".to_string()),
                at: now_iso(),
            },
        );

        let json = state.to_json(&[], &[]);
        let snapshot: Snapshot = serde_json::from_str(&json).expect("reader parses writer output");
        assert_eq!(snapshot.auto_installs.len(), 1, "latest state per server");
        let entry = &snapshot.auto_installs[0];
        assert_eq!(entry.server, "gopls");
        assert_eq!(entry.status, "failed");
        assert_eq!(entry.detail.as_deref(), Some("registry unreachable"));
    }

    #[tokio::test]
    async fn writer_record_bridge_mismatch_sets_then_clears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::with_coalesce(
            &tokio::runtime::Handle::current(),
            dir.path(),
            daemon_info(),
            Duration::from_millis(0),
        );

        // A disagreeing hello records the mismatch onto the persisted snapshot.
        writer.record_bridge_mismatch(Some("2.0.1"), "2.0.2");
        writer.flush_now();
        assert!(
            poll_until(|| {
                read_snapshot(&writer)
                    .and_then(|v| v.get("daemon")?.get("bridge_mismatch").cloned())
                    .is_some()
            })
            .await,
            "a mismatch is recorded onto the snapshot",
        );

        // An agreeing hello clears it — the persistent surfaces go silent.
        writer.record_bridge_mismatch(Some("2.0.2"), "2.0.2");
        writer.flush_now();
        assert!(
            poll_until(|| {
                read_snapshot(&writer)
                    .and_then(|v| v.get("daemon")?.get("bridge_mismatch").cloned())
                    .is_none()
            })
            .await,
            "agreement clears the record",
        );
    }

    #[test]
    fn record_activity_dedups_files_and_counts_distinct() {
        let mut state = fresh_state();
        // A repeat touch of the same file is not a change (no flush storm).
        assert!(state.record_activity("rust", "/p", "src/a.rs"));
        assert!(!state.record_activity("rust", "/p", "src/a.rs"));
        assert!(state.record_activity("rust", "/p", "src/b.rs"));
        let langs = state.activity_languages();
        assert_eq!(langs.len(), 1);
        assert_eq!(langs[0].file_count, 2, "distinct files counted");
        assert_eq!(langs[0].files, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn record_activity_separates_language_and_root_buckets() {
        let mut state = fresh_state();
        state.record_activity("rust", "/p", "a.rs");
        state.record_activity("cmake", "/p", "CMakeLists.txt");
        state.record_activity("rust", "/q", "b.rs");
        let langs = state.activity_languages();
        assert_eq!(langs.len(), 3, "each (language, root) is its own bucket");
    }

    #[test]
    fn forget_root_activity_prunes_only_the_named_root() {
        // Bug 93: a landed/removed root's provenance buckets must leave the
        // ledger so the doctor stops rendering `routed by … in <removed root>`.
        // Buckets under other roots survive; the target root's — across every
        // language — are dropped.
        let mut state = fresh_state();
        state.record_activity("rust", "/gone", "a.rs");
        state.record_activity("cmake", "/gone", "CMakeLists.txt");
        state.record_activity("rust", "/stay", "b.rs");

        assert!(
            state.forget_root_activity("/gone"),
            "pruning a root with buckets reports a change",
        );
        let langs = state.activity_languages();
        assert_eq!(langs.len(), 1, "only the surviving root's bucket remains");
        assert_eq!(langs[0].root, "/stay");

        assert!(
            !state.forget_root_activity("/gone"),
            "pruning an already-absent root is a no-op (no spurious flush)",
        );
        assert!(
            !state.forget_root_activity("/never-seen"),
            "pruning a root that was never recorded reports no change",
        );
    }

    #[test]
    fn daemon_current_version_matches_the_skew_source() {
        // Regression (tui-rework 09, item 1): the daemon snapshot must record the
        // same binary version the skew check compares against, so a non-tag build
        // never reads as falsely skewed. Both source it from `BINARY_VERSION`.
        let daemon = DaemonInfo::current("daemon:x".to_string(), 7, now_iso());
        assert_eq!(daemon.version, crate::health::skew::BINARY_VERSION);
        assert_eq!(daemon.version, env!("CATENARY_VERSION"));
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
