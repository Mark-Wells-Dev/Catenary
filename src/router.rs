// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Daemon session manager and socket listeners.
//!
//! [`SessionManager`] is the core daemon component. It binds two Unix domain
//! sockets — one for MCP connections from `catenary bridge` proxies, one for
//! hook connections from `catenary hook` CLI processes — and tracks MCP
//! connections by file descriptor. Hook connections are short-lived
//! (one request-response each).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};

use crate::bridge::EditingGuardrail;
use crate::bridge::HookRouter;
use crate::bridge::filesystem_manager::Root;
use crate::bridge::session::Session;
use crate::bridge::{GlobOutcome, GrepFlags, GrepOutcome, GrepSkips, HunkChunk, ShapedOutput};
use crate::companions::expand_companions;
use crate::hook::{HookRequest, HookResponseEnvelope, emit_hook_event, hook_outcome_level};
use crate::logging::LoggingServer;
use crate::mcp::McpServer;
use crate::source::Source;

/// Returns the MCP socket path for the daemon.
///
/// The path is deterministic: `$XDG_STATE_HOME/catenary/catenary-mcp.sock`
/// (or platform equivalent via [`crate::paths::state_dir`]).
///
/// Only the bridge proxy connects to this socket — it carries MCP
/// JSON-RPC traffic between the host CLI and the daemon.
#[must_use]
pub fn mcp_socket_path() -> PathBuf {
    crate::paths::state_dir()
        .join("catenary")
        .join("catenary-mcp.sock")
}

/// Returns the general-purpose IPC socket path for the daemon.
///
/// The path is deterministic: `$XDG_STATE_HOME/catenary/catenary.sock`
/// (or platform equivalent via [`crate::paths::state_dir`]).
///
/// This socket carries all non-MCP daemon traffic: hook events
/// (`pre-tool/*`, `post-agent/*`, etc.) and CLI commands
/// (`editing-start`, `editing-stop`, `roots-add`, `roots-rm`,
/// `roots-ls`, `shutdown`).
#[must_use]
pub fn socket_path() -> PathBuf {
    crate::paths::state_dir()
        .join("catenary")
        .join("catenary.sock")
}

// ── IPC request/response types for CLI tool commands ─────────────

/// IPC method string for grep requests.
pub const METHOD_GREP: &str = "tool/grep";

/// IPC method string for glob requests.
pub const METHOD_GLOB: &str = "tool/glob";

/// Compact, lexically-sortable UTC timestamp prefix for per-invocation search
/// files (`grep/<ts>_<uuid>.jsonl`).
///
/// The firehose's per-tool reaper evicts oldest-first by this prefix, so it must
/// sort lexically — millisecond UTC, no separators (e.g. `20260609T143210123Z`).
fn search_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string()
}

/// CLI-side cwd string for a search record's project field; empty when the
/// caller reported no cwd.
fn search_cwd(cwd: Option<&Path>) -> String {
    cwd.map(|p| p.display().to_string()).unwrap_or_default()
}

/// IPC request payload for `catenary grep`.
///
/// Sent as a JSON line over the daemon IPC socket with
/// `"method": "tool/grep"`. [`to_params`](Self::to_params) resolves
/// relative paths and `exclude` patterns against `cwd` before
/// dispatching to the grep pipeline.
///
/// Wire format:
/// ```json
/// {"method": "tool/grep", "cwd": "/path", "pattern": "foo", "paths": ["src/main.rs"]}
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "1:1 with the clap-parsed grep flags plus the transport-only chunked capability"
)]
pub struct GrepRequest {
    /// Working directory from the CLI process.
    ///
    /// `None` when the caller has no meaningful cwd (e.g. test fixtures
    /// using `spawn_in_state`). When absent, the daemon falls back to
    /// searching all workspace roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Search pattern (regex, supports `|` for alternation).
    pub pattern: String,
    /// Literal file/directory paths to scope the search.
    ///
    /// All positional arguments are concrete filesystem paths — the
    /// shell is the only glob engine. These bypass glob matching and
    /// are used as direct search roots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathBuf>,
    /// Glob pattern to exclude from matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
    /// Include files ignored by `.gitignore`.
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden files and directories.
    #[serde(default)]
    pub include_hidden: bool,
    /// Return a match/file count instead of rendered results (`--count`).
    #[serde(default)]
    pub count: bool,
    /// Protocol capability (misc 140 phase 2): the CLI understands the chunked
    /// [`GrepFrame`] response stream. A daemon that predates framing ignores this
    /// unknown field and replies with the single-envelope [`GrepResponse`]; the
    /// CLI detects that by the absent frame tag and parses the legacy envelope.
    /// Absent (a legacy CLI) → the daemon replies with the single envelope, which
    /// the legacy CLI parses. Every version combination degrades honestly. The
    /// field is transport-only — never forwarded into the grep pipeline params.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chunked: bool,
    /// Ripgrep-parity flags (`-i`/`-s`/`-w`/`-F`/`-v`/`-l`, context, `-g`/
    /// `--type`). Flattened onto the wire so the request stays a flat object and
    /// a flagless query serializes exactly as before (each inner field carries
    /// its own `#[serde(default)]`, so a minimal payload still deserializes).
    #[serde(flatten)]
    pub flags: GrepFlags,
}

impl GrepRequest {
    /// Resolves relative paths against `cwd` and produces a
    /// `GrepInput`-compatible JSON value for the grep pipeline.
    ///
    /// - Paths are resolved against `cwd` (relative → absolute).
    /// - `exclude` is resolved against `cwd`.
    /// - `targets_hidden` is checked on paths to auto-enable
    ///   `include_hidden` for explicit hidden targets like `.gitignore`.
    fn to_params(&self) -> serde_json::Value {
        let mut include_hidden = self.include_hidden;

        let mut params = serde_json::json!({
            "pattern": self.pattern,
            "include_gitignored": self.include_gitignored,
            "count": self.count,
        });

        if self.paths.is_empty() {
            // No paths — cwd-scoped search. Pass cwd so the daemon
            // scopes to the agent's working directory.
            if let Some(ref cwd) = self.cwd {
                params["cwd"] = serde_json::Value::String(cwd.to_string_lossy().into_owned());
            }
        } else {
            // Literal paths — resolve relative paths against cwd,
            // check for hidden targeting.
            for p in &self.paths {
                let s = p.to_string_lossy();
                if !p.is_absolute() && crate::bridge::session::ResolvedGlob::targets_hidden(&s) {
                    include_hidden = true;
                }
            }
            params["paths"] = serde_json::Value::Array(
                self.paths
                    .iter()
                    .map(|p| {
                        let s = if p.is_absolute() {
                            p.to_string_lossy().into_owned()
                        } else {
                            self.cwd.as_ref().map_or_else(
                                || p.to_string_lossy().into_owned(),
                                |cwd| cwd.join(p).to_string_lossy().into_owned(),
                            )
                        };
                        serde_json::Value::String(s)
                    })
                    .collect(),
            );
        }
        if let Some(ref exclude) = self.exclude {
            let resolved = self
                .cwd
                .as_ref()
                .map_or_else(|| exclude.clone(), |cwd| resolve_relative(exclude, cwd));
            params["exclude"] = serde_json::Value::String(resolved);
        }
        params["include_hidden"] = serde_json::Value::Bool(include_hidden);

        // Merge the ripgrep-parity flags as flat keys so the daemon-side
        // `GrepInput` (which flattens the same `GrepFlags`) deserializes them.
        if let (Some(params_obj), Ok(serde_json::Value::Object(flag_obj))) =
            (params.as_object_mut(), serde_json::to_value(&self.flags))
        {
            for (k, v) in flag_obj {
                params_obj.insert(k, v);
            }
        }

        params
    }
}

/// IPC response for `catenary grep`.
///
/// Returned as a JSON line over the daemon IPC socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GrepResponse {
    /// Rendered grep output (empty for a `--count` response).
    pub output: String,
    /// Matching-line count, present only for a `--count` response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<usize>,
    /// Distinct-file count, present only for a `--count` response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    /// Files in the search scope skipped instead of searched (misc 135, bug 62).
    /// Empty for the common all-searched query, so a normal response is byte-for-
    /// byte unchanged on the wire (the field is omitted when empty).
    #[serde(default, skip_serializing_if = "GrepSkips::is_empty")]
    pub skipped: GrepSkips,
}

/// One frame of a chunked `catenary grep` response (misc 140 phase 2).
///
/// The framed grep response is a stream of [`GrepFrame::Chunk`] frames — each
/// carrying one file's rendered hunk, in global (file, line) sort order —
/// terminated by exactly one [`GrepFrame::End`] frame carrying the tallies the
/// single-envelope [`GrepResponse`] used to carry (count totals, skip records).
/// The CLI concatenates the chunk payloads into the rendered output and reads
/// the terminator for the metadata, reproducing the pre-framing response
/// byte-for-byte.
///
/// Each frame serializes to one JSON line, exactly like the pre-framing envelope,
/// so the transport is unchanged. The internally-tagged `"frame"` field is the
/// version-skew hinge: it distinguishes a frame from a legacy single-envelope
/// response (which has no `"frame"` key), and an unrecognized tag deserializes to
/// a comprehensible error rather than a silent misparse.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum GrepFrame {
    /// A slice of the rendered output — one file's hunk — in sort order.
    Chunk {
        /// UTF-8 output bytes for this chunk.
        data: String,
    },
    /// The terminator, carrying the same tallies as [`GrepResponse`].
    End {
        /// Matching-line count, present only for a `--count` response.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        matches: Option<usize>,
        /// Distinct-file count, present only for a `--count` response.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<usize>,
        /// Files in the search scope skipped instead of searched.
        #[serde(default, skip_serializing_if = "GrepSkips::is_empty")]
        skipped: GrepSkips,
    },
}

/// IPC request payload for `catenary glob`.
///
/// Sent as a JSON line over the daemon IPC socket with
/// `"method": "tool/glob"`. The daemon resolves relative paths
/// against `cwd` before dispatching to the glob pipeline.
///
/// Wire format:
/// ```json
/// {"method": "tool/glob", "cwd": "/path", "paths": ["src/"]}
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobRequest {
    /// Working directory from the CLI process.
    ///
    /// `None` when the caller has no meaningful cwd. When absent, the
    /// daemon falls back to searching all workspace roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Literal file/directory paths.
    ///
    /// All positional arguments are concrete filesystem paths — the
    /// shell is the only glob engine. Each is dispatched through the
    /// appropriate handler (file outline, directory listing).
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Glob pattern to exclude from results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
    /// Include files ignored by `.gitignore`.
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden files and directories.
    #[serde(default)]
    pub include_hidden: bool,
    /// Return a path count instead of rendered results (`--count`).
    #[serde(default)]
    pub count: bool,
}

impl GlobRequest {
    /// Resolves relative paths against `cwd` and produces a
    /// `GlobInput`-compatible JSON value for the glob pipeline.
    ///
    /// - Paths are resolved against `cwd` (relative → absolute).
    /// - `targets_hidden` is checked on paths to auto-enable
    ///   `include_hidden` for explicit hidden targets.
    /// - Basename `exclude` patterns (no `/`) get a `**/` prefix for
    ///   depth-independent matching; patterns with `/` are resolved
    ///   against `cwd`.
    fn to_params(&self) -> serde_json::Value {
        let mut include_hidden = self.include_hidden;

        let mut params = serde_json::json!({
            "include_gitignored": self.include_gitignored,
            "count": self.count,
        });

        // Check for hidden targeting on relative paths.
        for p in &self.paths {
            let s = p.to_string_lossy();
            if !p.is_absolute() && crate::bridge::session::ResolvedGlob::targets_hidden(&s) {
                include_hidden = true;
            }
        }

        // Resolve relative paths against cwd.
        params["paths"] = serde_json::Value::Array(
            self.paths
                .iter()
                .map(|p| {
                    let s = if p.is_absolute() {
                        p.to_string_lossy().into_owned()
                    } else {
                        self.cwd.as_ref().map_or_else(
                            || p.to_string_lossy().into_owned(),
                            |cwd| cwd.join(p).to_string_lossy().into_owned(),
                        )
                    };
                    serde_json::Value::String(s)
                })
                .collect(),
        );
        params["include_hidden"] = serde_json::Value::Bool(include_hidden);

        // Preserve each argument's original spelling (pre-absolutization) so the
        // glob pipeline can echo a pattern in a cardinality header exactly as the
        // agent typed it (misc 121) — the same original-spelling contract the
        // zero-match report uses. 1:1 with `params["paths"]` above.
        params["display_paths"] = serde_json::Value::Array(
            self.paths
                .iter()
                .map(|p| serde_json::Value::String(p.to_string_lossy().into_owned()))
                .collect(),
        );

        if let Some(ref exclude) = self.exclude {
            let effective = if exclude.contains('/') {
                self.cwd
                    .as_ref()
                    .map_or_else(|| exclude.clone(), |cwd| resolve_relative(exclude, cwd))
            } else {
                format!("**/{exclude}")
            };
            params["exclude"] = serde_json::Value::String(effective);
        }
        params
    }
}

/// IPC response for `catenary glob`.
///
/// Returned as a JSON line over the daemon IPC socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobResponse {
    /// Rendered glob output (empty for a `--count` response).
    pub output: String,
    /// Resolved-path count, present only for a `--count` response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<usize>,
    /// Glob-pattern arguments (original spelling) that expanded to zero
    /// matches. The CLI renders each as a loud
    /// `no matches for pattern: <pattern>` line (misc 118). Empty for a
    /// `--count` response and for any query where every pattern matched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_match_patterns: Vec<String>,
}

/// Resolves a pattern path against a base directory if it is relative.
///
/// Tilde-expands the pattern first. Absolute paths and `~` paths are
/// returned as-is. Relative paths are joined to `base`.
fn resolve_relative(pattern: &str, base: &Path) -> String {
    let expanded = crate::bridge::expand_tilde(pattern);
    if Path::new(&expanded).is_absolute() {
        return expanded;
    }
    base.join(&expanded).to_string_lossy().into_owned()
}

/// Pre-bound MCP and IPC socket listeners.
///
/// Returned by [`bind_daemon_sockets`] for early socket binding in daemon
/// mode. Pass to [`SessionManager::from_listeners`] once the tool handler
/// is ready.
#[cfg(unix)]
pub struct DaemonSockets {
    /// MCP socket listener.
    pub mcp_listener: tokio::net::UnixListener,
    /// General-purpose IPC socket listener.
    pub ipc_listener: tokio::net::UnixListener,
    /// Filesystem path of the MCP socket.
    pub mcp_path: PathBuf,
    /// Filesystem path of the IPC socket.
    pub ipc_path: PathBuf,
}

/// Binds the daemon's MCP and IPC sockets immediately.
///
/// Call this early in daemon startup so that bridge proxies can connect
/// while heavy initialization (config loading, LSP spawning) proceeds.
/// The kernel queues incoming connections until [`SessionManager::accept_loop`]
/// starts processing them.
///
/// # Errors
///
/// Returns an error if directories cannot be created or sockets cannot
/// be bound.
#[cfg(unix)]
pub fn bind_daemon_sockets() -> Result<DaemonSockets> {
    let mcp_path = mcp_socket_path();
    let ipc_path = socket_path();

    if let Some(parent) = mcp_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory: {}", parent.display()))?;
    }
    if let Some(parent) = ipc_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory: {}", parent.display()))?;
    }

    let mcp_listener = tokio::net::UnixListener::bind(&mcp_path)
        .with_context(|| format!("bind MCP socket: {}", mcp_path.display()))?;
    let ipc_listener = tokio::net::UnixListener::bind(&ipc_path)
        .with_context(|| format!("bind IPC socket: {}", ipc_path.display()))?;

    info!(
        source = Source::DaemonLifecycle.as_str(),
        mcp_path = %mcp_path.display(),
        ipc_path = %ipc_path.display(),
        "daemon sockets bound",
    );

    Ok(DaemonSockets {
        mcp_listener,
        ipc_listener,
        mcp_path,
        ipc_path,
    })
}

// ── Session registry ───────────────────────────────────────────────

/// Per-session state: the [`HookRouter`] (which owns the `Session`) plus the
/// board metadata captured at session creation.
#[cfg(unix)]
struct SessionEntry {
    router: Arc<HookRouter>,
    /// Identity captured from the session's first hook payload, surfaced on
    /// the snapshot session board (observability ticket 05).
    meta: SessionMeta,
}

/// Snapshot session-board metadata, captured from a session's own hook payload
/// at creation time.
///
/// `status`, `last_action`, and `last_seen` are *not* here — they are read live
/// from the per-session [`Session`] at snapshot-build time (status derived from
/// the editing accumulator; `last_action` and `last_seen` stored on the
/// session). Only the create-time identity that the payload carries lives here.
#[cfg(unix)]
#[derive(Clone)]
struct SessionMeta {
    /// Host CLI name from the hook `format` field (`claude`/`antigravity`/…).
    client_name: Option<String>,
    /// When the session first connected (ISO 8601).
    started_at: String,
    /// Workspace roots from the session's own payload (`cwd` /
    /// `workspacePaths`) — never correlated to MCP roots.
    roots: Vec<String>,
}

/// Extracts a session's workspace roots from its hook payload.
///
/// Host-agnostic: Antigravity sends `workspacePaths` (array), Claude Code sends
/// `cwd` (string). Returns an empty vec when neither is
/// present. Deliberately reads only the session's *own* payload — per the
/// design, the board does not correlate `session_id` to MCP roots.
#[cfg(unix)]
fn extract_session_roots(raw: &serde_json::Value) -> Vec<String> {
    let Some(hp) = raw.get("host_payload") else {
        return Vec::new();
    };
    if let Some(paths) = hp.get("workspacePaths").and_then(|v| v.as_array()) {
        return paths
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    hp.get("cwd")
        .and_then(|v| v.as_str())
        .map(|cwd| vec![cwd.to_string()])
        .unwrap_or_default()
}

/// Live session board over the daemon's per-session registry.
///
/// Implements [`crate::state_snapshot::SessionBoard`]: at each snapshot flush
/// it walks the live sessions and builds one entry each, deriving `status`
/// from the editing accumulator and reading `last_action` off the session.
/// Wired onto the [`SnapshotWriter`](crate::state_snapshot::SnapshotWriter) in
/// [`SessionManager::with_session`].
#[cfg(unix)]
struct SessionBoardImpl {
    sessions: Arc<std::sync::Mutex<HashMap<String, SessionEntry>>>,
    /// Live subagents by parent session, pulled at each flush so a session's
    /// board entry carries its running subagent sub-rows (tui-rework 03).
    subagents: SubagentRegistry,
}

#[cfg(unix)]
impl crate::state_snapshot::SessionBoard for SessionBoardImpl {
    fn sessions(&self) -> Vec<crate::state_snapshot::SessionEntry> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions
            .iter()
            .map(|(id, entry)| crate::state_snapshot::SessionEntry {
                id: id.clone(),
                client: crate::state_snapshot::ClientInfo {
                    name: entry
                        .meta
                        .client_name
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    // The hook payloads Catenary receives carry no version.
                    version: None,
                },
                started_at: entry.meta.started_at.clone(),
                last_seen: entry.router.session.last_seen(),
                roots: entry.meta.roots.clone(),
                status: entry.router.session.status(),
                last_action: entry.router.session.last_action(),
                // Enrich each subagent with its own per-`(session, agent)` batch
                // status, derived from the parent session's editing manager
                // (tui-rework 14, item 3). `last_seen` stays the subagent's
                // start time until a per-subagent recency signal is plumbed —
                // the render falls back to it, so no status is fabricated.
                subagents: self
                    .subagents
                    .for_session(id)
                    .into_iter()
                    .map(|mut sub| {
                        sub.status = entry.router.session.subagent_status(&sub.id);
                        sub
                    })
                    .collect(),
            })
            .collect()
    }
}

/// Daemon-side live-subagent registry: parent `session_id` → its running
/// subagents.
///
/// Populated at `SubagentStart` and pruned at `SubagentStop` / `SessionEnd`;
/// the session board pulls it at each snapshot flush. Independent of the
/// session-entry lifecycle (a subagent can be recorded whether or not the
/// parent has a live entry yet), mirroring the `worktree_mounts` /
/// `ephemeral_mounts` side registries. Only hosts that feed subagent identity
/// (Claude Code) ever populate it — capability-aware, no fabrication.
#[cfg(unix)]
#[derive(Clone, Default)]
struct SubagentRegistry {
    inner: Arc<std::sync::Mutex<HashMap<String, Vec<crate::state_snapshot::Subagent>>>>,
}

#[cfg(unix)]
impl SubagentRegistry {
    /// A fresh, empty registry.
    fn new() -> Self {
        Self::default()
    }

    /// Record a subagent under its parent session (idempotent per agent id).
    /// A blank agent id (path-keyed `--worktree` session) records nothing.
    fn start(&self, session_id: &str, agent_id: &str, started_at: String) {
        if agent_id.is_empty() {
            return;
        }
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let list = map.entry(session_id.to_string()).or_default();
        if list.iter().all(|s| s.id != agent_id) {
            // `status` / `last_seen` are enriched at snapshot-build time from the
            // parent session's batch (tui-rework 14, item 3); the registry seeds
            // their defaults here.
            list.push(crate::state_snapshot::Subagent {
                id: agent_id.to_string(),
                started_at,
                ..crate::state_snapshot::Subagent::default()
            });
        }
        drop(map);
    }

    /// Remove a subagent at stop; drops the session bucket when it empties.
    fn stop(&self, session_id: &str, agent_id: &str) {
        if agent_id.is_empty() {
            return;
        }
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(list) = map.get_mut(session_id) {
            list.retain(|s| s.id != agent_id);
            if list.is_empty() {
                map.remove(session_id);
            }
        }
    }

    /// Drop every subagent under a session (its `SessionEnd` sweep).
    fn clear_session(&self, session_id: &str) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(session_id);
    }

    /// The live subagents under `session_id`, sorted by start time.
    fn for_session(&self, session_id: &str) -> Vec<crate::state_snapshot::Subagent> {
        let mut list = {
            let map = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.get(session_id).cloned().unwrap_or_default()
        };
        list.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        list
    }
}

/// The daemon-level [`crate::state_snapshot::RootBoard`] source, backed by the
/// live [`RootTracker`]. Pulled at each snapshot flush so `state.json` carries
/// the current tracked roots with their class (ephemeral-roots ticket 02).
#[cfg(unix)]
struct RootBoardImpl {
    tracker: RootTracker,
    /// The ephemeral idle clocks, so the board can surface each activity-mounted
    /// root's idle-remaining figure alongside its class.
    ephemeral_mounts: EphemeralMounts,
}

#[cfg(unix)]
impl crate::state_snapshot::RootBoard for RootBoardImpl {
    fn roots(&self) -> Vec<crate::state_snapshot::RootEntry> {
        let now = Instant::now();
        self.tracker
            .list_roots()
            .into_iter()
            .map(|(path, sources)| {
                // The full contributor classes (no longer collapsed to the
                // `ephemeral` bool) plus the idle-remaining figure for an
                // activity-mounted root — the same view `tool/roots-ls` reports.
                let idle_remaining_secs = self
                    .ephemeral_mounts
                    .idle_remaining(&path, now, EPHEMERAL_ROOT_IDLE_TIMEOUT)
                    .map(|d| d.as_secs());
                crate::state_snapshot::RootEntry {
                    path: path.display().to_string(),
                    ephemeral: root_is_ephemeral(&sources),
                    sources,
                    idle_remaining_secs,
                }
            })
            .collect()
    }
}

/// Classifies a tracked root as ephemeral from its contributor sources.
///
/// A root is ephemeral iff **every** contributor holding it is an
/// `ephemeral:*` key — i.e. it is held only by activity mounts. Any pinned
/// contributor (`hook` / `mcp:*` / `worktree:*`) makes it pinned, which is why
/// `catenary pin` upgrades a root by adding a `hook` contributor (and this feature
/// then drops the ephemeral one). An empty source list is never ephemeral.
#[cfg(unix)]
fn root_is_ephemeral(sources: &[String]) -> bool {
    !sources.is_empty()
        && sources
            .iter()
            .all(|s| s.starts_with(EPHEMERAL_CONTRIBUTOR_PREFIX))
}

/// Bounds concurrent `catenary grep`/`glob` walks daemon-wide so one session's
/// monster search cannot starve concurrent sessions (misc 140 phase 2, decision
/// 029 §5 — a daemon-side guard, invisible to any single caller's output).
///
/// A shared, FIFO [`tokio::sync::Semaphore`]: each search acquires one permit for
/// the duration of its walk and releases it before streaming results, so a burst
/// of sessions can never pile up unbounded parallel walks that thrash the daemon
/// — excess searches queue fairly and the runtime keeps serving other sessions'
/// requests. The permit count leaves headroom below saturation.
#[cfg(unix)]
#[derive(Clone)]
struct SearchLimiter {
    semaphore: Arc<tokio::sync::Semaphore>,
}

#[cfg(unix)]
impl SearchLimiter {
    /// Creates a limiter with `permits` concurrent-search slots (at least one).
    fn new(permits: usize) -> Self {
        Self {
            semaphore: Arc::new(tokio::sync::Semaphore::new(permits.max(1))),
        }
    }

    /// A limiter sized to the host's parallelism — the default daemon guard.
    fn with_default_permits() -> Self {
        let permits = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        Self::new(permits)
    }

    /// Acquires a search permit, awaiting a free slot. The owned permit is held
    /// for the walk's duration and released on drop. `None` only if the
    /// semaphore were closed (it never is) — the caller then proceeds unlimited,
    /// which is safe.
    async fn acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.semaphore).acquire_owned().await.ok()
    }
}

/// Shared context for session-aware hook dispatch.
///
/// When set on [`SessionManager`], hook connections are routed to
/// per-`session_id` [`Session`] + [`HookRouter`] pairs. Each session
/// has independent editing state and turn counter. Heavy resources
/// (`LspClientManager`, config, logging) are shared via `Arc` from the
/// daemon's primary session. When absent, hooks receive passthrough
/// responses (allow everything).
#[cfg(unix)]
#[derive(Clone)]
struct HookDispatchContext {
    /// Per-`session_id` session entries. Each entry has its own
    /// `Session` (per-session state) and `HookRouter` (turn counter,
    /// debounce).
    sessions: Arc<std::sync::Mutex<HashMap<String, SessionEntry>>>,
    /// Bounds concurrent grep/glob walks so one session's monster search cannot
    /// starve the others (misc 140 phase 2).
    search_limiter: SearchLimiter,
    /// Daemon's primary session — used as the template for creating
    /// per-session sessions via [`Session::new_for_daemon`].
    primary: Arc<Session>,
    /// Logging server for sink access.
    _logging: LoggingServer,
    /// Root tracker for refcount-aware root management across sessions.
    root_tracker: Option<RootTracker>,
    /// Cross-session per-root editing guardrail. Shared with all
    /// per-session `Session` instances to prevent concurrent editing
    /// in the same workspace root.
    editing_guardrail: Arc<EditingGuardrail>,
    /// Bounded directory-deletion watch for mounted worktree roots (ticket 05).
    /// Registered at `SubagentStart` mount, unregistered on every teardown path
    /// (`WorktreeRemove`, `SessionEnd` sweep, the GC reap). Reaps the
    /// `worktree:{session_id}:{path}` root the instant the worktree dir is deleted
    /// — `git worktree remove` fires no `WorktreeRemove` hook. `None` when the OS
    /// watcher couldn't be created (the hourly GC remains the backstop) or in
    /// transport-only test managers.
    worktree_watcher: Option<crate::worktree_watch::WorktreeWatcher>,
    /// Per-key hook→CLI handoff (ADR 014). Replaces the single global slot +
    /// 1-permit semaphore: each [`HandoffKey`] serializes independently, so a
    /// staged `diagnostics` handoff never stalls another session daemon-wide.
    handoff: KeyedHandoff,
    /// Per-root idle clock for activity-mounted ephemeral roots (ephemeral-roots
    /// ticket 02). A CLI query touching a path outside every mounted root mounts
    /// its enclosing project root under an `ephemeral:*` contributor and records
    /// activity here; the idle reaper reads it to tear the mount down.
    ephemeral_mounts: EphemeralMounts,
    /// Daemon-side first-sighting ledger for the Antigravity `PreInvocation`
    /// teaching injection (teaching-surface ticket 03). Records each
    /// `conversationId` the hook has taught, so the persisted `userMessage` is
    /// injected exactly once per conversation.
    first_sightings: FirstSightings,
    /// Identity→(path, metadata) registry for Catenary-created worktrees
    /// (misc 150). Registered at `worktree-create/log-payload`, rehydrated from
    /// sidecars at startup; anchors the identity-keyed `SubagentStop` reap and the
    /// `worktree-remove` reverse lookup, and carries misc 151's disposal metadata.
    worktree_registry: WorktreeRegistry,
    /// Per-root idle clock + blocked-on-permission flag for mounted worktree-class
    /// roots (misc 150). Refreshed by the same qualifying activities that touch
    /// the ephemeral clocks; the worktree idle reaper reads it to unmount a quiet
    /// worktree root, and a blocked root is exempt from that expiry.
    worktree_mounts: WorktreeMounts,
    /// Live subagents by parent session (tui-rework 03). Recorded at
    /// `SubagentStart`, pruned at `SubagentStop` / `SessionEnd`; shared with the
    /// session board so `state.json` carries subagent sub-rows.
    subagents: SubagentRegistry,
}

/// A staged hook→CLI handoff, deposited under a [`HandoffKey`] by the
/// `PreToolUse` hook and consumed by the matching CLI command.
///
/// The payload (see [`HandoffPayload`]) is *data-back*: `diagnostics` snapshots
/// the drained file set for the CLI command to retrieve.
///
/// Dropping this struct drops the owned semaphore permit, releasing the key's
/// serialization lock for the next same-key stage.
struct HandoffContext {
    /// Scope UUID minted at prepare time. Used as `parent_id` for the
    /// IPC request/response events and all LSP children, linking them into
    /// one TUI scope.
    parent_id: String,
    /// The staged payload, keyed by direction.
    payload: HandoffPayload,
    /// Owned semaphore permit — dropped when the `HandoffContext`
    /// is dropped (slot consumed or timeout), releasing the per-key lock.
    /// Never read directly; held purely for RAII drop semantics.
    #[allow(dead_code, reason = "RAII guard — held for drop, not read")]
    permit: tokio::sync::OwnedSemaphorePermit,
}

/// The payload of a staged [`HandoffContext`].
enum HandoffPayload {
    /// `diagnostics` — *data-back*: the hook snapshots the batch and the
    /// `catenary diagnostics` CLI command retrieves it. The batch is never
    /// mutated at prepare *or* consume until delivery (misc 141): its `delivered`
    /// flags flip only after the response's socket write succeeds, so a failed
    /// attempt that never delivers leaves the batch and its gate intact for a
    /// retry. The `editing_session`/`agent_id` key rides the handoff so the flip
    /// targets the right `EditingManager` bucket.
    Diagnostics {
        /// The batch's files snapshotted from the editing session (delivered
        /// ones included — a bare pull re-diagnoses the whole batch).
        files: Vec<PathBuf>,
        /// Number of files skipped because they were outside tracked workspace
        /// roots (no LSP coverage).
        filtered: usize,
        /// Distinct enclosing project roots of those filtered edits, for the
        /// root-aware bare-run note (ephemeral-roots ticket 01 / bug 58). Empty
        /// when no filtered edit had a detectable enclosing root.
        filtered_roots: std::collections::BTreeSet<PathBuf>,
        /// Host session id (from the staging hook). The bare `catenary
        /// diagnostics` process is identity-less, so the session id rides the
        /// handoff — the daemon looks the session up to flip its batch.
        session_id: String,
        /// `EditingManager` session key (the raw `session_id` Option, absent →
        /// `None`) used to flip the batch on delivery. Distinct from the
        /// `"default"`-fallback `session_id` above; mirrors how the prepare
        /// snapshot was keyed.
        editing_session: Option<String>,
        /// Agent id used to flip the batch's per-agent bucket on delivery
        /// (bug 37 scoping — touch only the requesting agent's batch).
        agent_id: String,
    },
}

/// Correlation key for the hook→CLI handoff — the catenary subcommand alone
/// (ADR 014).
///
/// `cwd`, pattern, and path are recorded for observability bucketing but are
/// *not* key material. Only the load-bearing, bare-only `diagnostics` command
/// stages a handoff; stateless `grep`/`glob` self-scope with a daemon-minted
/// UUID and never correlate here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum HandoffKey {
    /// `catenary diagnostics` — data-back: the hook stages the accumulated
    /// file set, the CLI drains it.
    Diagnostics,
}

impl HandoffKey {
    /// Every handoff key — used to eagerly create the per-key semaphores.
    /// Cardinality 1 today (ADR 014); the registry stays keyed so a future
    /// correlated command plugs into the mechanism rather than rebuilding it.
    const ALL: [Self; 1] = [Self::Diagnostics];
}

/// Per-key handoff self-heal timeout.
///
/// Clears a staged handoff the CLI never consumes — e.g. the host killed the
/// `catenary diagnostics` subprocess between `PreToolUse` and
/// command execution — so a stuck stage can't hold its key's permit forever.
/// Scoped to one [`HandoffKey`] (ADR 014): clearing frees only the *next
/// same-key* handoff, never a daemon-wide stall.
///
/// The live stage→consume path is a subprocess **spawn + socket connect**,
/// which on a loaded machine can take well over a second. An earlier sub-second
/// bound falsely expired *live* handoffs under heavy parallel load — the
/// diagnostics integration test flaked with "handoff expired", and a real user
/// on a busy box could get that instead of diagnostics. The bound is therefore
/// generous: erring long is cheap (it only delays the next same-key handoff
/// after a genuinely abandoned stage, which is rare), so 10s leaves ample
/// headroom over the worst-case spawn while still self-healing in bounded time.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-key hook→CLI handoff registry (ADR 014).
///
/// Replaces the single global slot + 1-permit semaphore. Each [`HandoffKey`]
/// gets its own 1-permit semaphore — **same-key in-order serialization** (a
/// second `diagnostics` stage blocks until the first is consumed; no
/// overwrite, no double-consume) — and its own slot + timeout, so a stuck
/// stage self-heals without stalling the daemon as the old global lock could.
/// Cardinality 1 today; the registry stays keyed for future correlated
/// commands.
#[derive(Clone)]
struct KeyedHandoff {
    /// Per-key serialization semaphores (1 permit each), created eagerly for
    /// every [`HandoffKey`]. A stage acquires its key's permit; the owned
    /// permit rides inside the staged [`HandoffContext`] and releases on
    /// consume or timeout (RAII).
    semaphores: Arc<HashMap<HandoffKey, Arc<tokio::sync::Semaphore>>>,
    /// Per-key staged contexts. A staged handoff lives here until the CLI
    /// consumes it or the per-key timeout clears it.
    slots: Arc<std::sync::Mutex<HashMap<HandoffKey, HandoffContext>>>,
    /// Self-heal timeout for a staged-but-unconsumed handoff. Production uses
    /// [`HANDOFF_TIMEOUT`] (via [`Self::new`]); tests inject a short value via
    /// [`Self::with_timeout`] to exercise the clear-on-timeout path quickly.
    timeout: Duration,
}

impl KeyedHandoff {
    /// Build the registry with one 1-permit semaphore per [`HandoffKey`] and the
    /// production self-heal timeout ([`HANDOFF_TIMEOUT`]).
    fn new() -> Self {
        Self::with_timeout(HANDOFF_TIMEOUT)
    }

    /// Build the registry with an explicit self-heal timeout. [`Self::new`] is
    /// the production entry point; tests pass a short timeout to drive the
    /// clear-on-timeout path without a real-time wait.
    fn with_timeout(timeout: Duration) -> Self {
        let semaphores: HashMap<HandoffKey, Arc<tokio::sync::Semaphore>> = HandoffKey::ALL
            .into_iter()
            .map(|key| (key, Arc::new(tokio::sync::Semaphore::new(1))))
            .collect();
        Self {
            semaphores: Arc::new(semaphores),
            slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            timeout,
        }
    }

    /// Acquire `key`'s serialization permit. Blocks while another handoff under
    /// the **same** key is in flight; independent across keys. The returned
    /// permit must be moved into the staged [`HandoffContext`] so it releases
    /// on consume/timeout.
    async fn acquire(&self, key: HandoffKey) -> Result<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = self
            .semaphores
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("no handoff semaphore for {key:?}"))?;
        semaphore
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("handoff semaphore closed"))
    }

    /// Deposit `context` under `key` and arm its per-key timeout. The caller
    /// already holds `key`'s permit (inside `context`), so at most one context
    /// per key is ever live.
    fn stage(&self, key: HandoffKey, context: HandoffContext) {
        {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.insert(key, context);
        }
        self.spawn_timeout(key);
    }

    /// Take the staged context for `key`, releasing its permit (the returned
    /// context owns the permit; dropping it unblocks the next same-key stage).
    /// Returns `None` when nothing is staged — timed out or already consumed.
    fn consume(&self, key: HandoffKey) -> Option<HandoffContext> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key)
    }

    /// Spawn a background task that clears `key`'s slot after [`HANDOFF_TIMEOUT`]
    /// if the CLI never connects (e.g., the host kills the subprocess between
    /// `PreToolUse` and command execution). Dropping the cleared
    /// [`HandoffContext`] releases the key's permit. Scoped to `key` — never a
    /// daemon-wide stall.
    fn spawn_timeout(&self, key: HandoffKey) {
        let slots = self.slots.clone();
        let timeout = self.timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            // Remove under the lock, then drop the guard (and the removed
            // HandoffContext, releasing its permit) before logging.
            let cleared = {
                let mut slots = slots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slots.remove(&key).is_some()
            };
            if cleared {
                warn!(
                    source = Source::DaemonDispatch.as_str(),
                    handoff_key = ?key,
                    "handoff timeout — discarding staged context",
                );
            }
        });
    }
}

/// How often the daemon runs the periodic worktree-root GC
/// ([`SessionManager::spawn_worktree_root_gc`]).
///
/// Hourly, mirroring the firehose staleness sweep
/// ([`crate::logging::reaper::STALENESS_SWEEP_INTERVAL`]): a missed
/// `WorktreeRemove` leaks a single root, not a wedge risk, so a coarse cadence
/// reclaims it without putting `.exists()` probes on any hot path.
pub const WORKTREE_ROOT_GC_INTERVAL: Duration = Duration::from_hours(1);

/// Tracks per-contributor workspace root sets for reference counting.
///
/// Each MCP connection and CLI command contributes a set of roots
/// keyed by a contributor string. The global root set (union of all
/// contributors) is synced to the shared [`crate::lsp::LspClientManager`]
/// after each mutation. When a contributor is removed (MCP disconnect),
/// its roots leave the union — roots that no other contributor provides
/// have their per-root server instances shut down.
///
/// Contributor keys:
/// - `"mcp:{fd}"` — roots from MCP `roots/list` for a connection
/// - `"hook"` — roots from `catenary add-root` CLI commands
/// - `"worktree:{session_id}:{canonical path}"` — a subagent's worktree mounted
///   at `SubagentStart` and torn down at `WorktreeRemove` (workstream 30, ticket
///   03); keyed by the canonical worktree path (not `agent_id`, which
///   `WorktreeRemove` does not carry), so the root's lifetime tracks the
///   worktree's. The `session_id` prefix lets a session-level sweep reclaim
///   leaked roots without enumerating paths.
#[cfg(unix)]
#[derive(Clone)]
struct RootTracker {
    inner: Arc<std::sync::Mutex<RootTrackerInner>>,
}

/// The two indices the tracker maintains together under one lock.
///
/// `contributors` is the **provenance** forward index (which connection
/// declared which paths); `roots` is the **identity + config** store, shared
/// across contributors. A `Root` is born when a path's refcount goes 0→1
/// (loading `.catenary.toml` then) and reaped when it goes 1→0 — both happen in
/// [`reconcile_roots`](RootTrackerInner::reconcile_roots), kept in lock-step
/// with every contributor mutation, so a tracked root is always config-complete
/// (ticket 00a).
#[cfg(unix)]
struct RootTrackerInner {
    /// Per-contributor declared root sets. The global root set is the union of
    /// all values.
    contributors: HashMap<String, HashSet<PathBuf>>,
    /// Canonical config-complete roots, keyed by path, shared across
    /// contributors. Reconciled to the contributor union on every mutation.
    roots: HashMap<PathBuf, Arc<Root>>,
}

#[cfg(unix)]
impl RootTrackerInner {
    /// Reconciles the `roots` store to the current contributor union: births a
    /// config-loaded [`Root`] for each newly-present path (refcount 0→1) and
    /// reaps any path no contributor declares any more (refcount 1→0).
    ///
    /// Loading `.catenary.toml` happens here, exactly once per path's lifetime
    /// (the `entry`/`or_insert_with` skips paths already born) — so a steady
    /// re-sync that adds no new path does no I/O.
    fn reconcile_roots(&mut self) {
        let union: HashSet<PathBuf> = self.contributors.values().flatten().cloned().collect();
        self.roots.retain(|path, _| union.contains(path));
        for path in union {
            self.roots
                .entry(path.clone())
                .or_insert_with(|| Arc::new(Root::load(path)));
        }
    }
}

#[cfg(unix)]
impl RootTracker {
    fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(RootTrackerInner {
                contributors: HashMap::new(),
                roots: HashMap::new(),
            })),
        }
    }

    /// Replaces a contributor's root set.
    fn set_roots(&self, contributor: &str, roots: Vec<PathBuf>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .contributors
            .insert(contributor.to_string(), roots.into_iter().collect());
        inner.reconcile_roots();
    }

    /// Adds roots to a contributor's set (does not remove existing ones).
    fn add_roots(&self, contributor: &str, roots: &[PathBuf]) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .contributors
            .entry(contributor.to_string())
            .or_default()
            .extend(roots.iter().cloned());
        inner.reconcile_roots();
    }

    /// Removes a contributor entirely.
    fn remove_contributor(&self, contributor: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.contributors.remove(contributor);
        inner.reconcile_roots();
    }

    /// Returns whether `contributor` currently contributes any roots.
    ///
    /// Lets a teardown caller (`SubagentStop`) run the reap only for a live
    /// worktree mount, so a stop whose `cwd` never mounted a worktree is a true
    /// no-op — no re-sync, no misleading teardown log.
    fn has_contributor(&self, contributor: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contributors
            .contains_key(contributor)
    }

    /// Removes every contributor whose key `starts_with(prefix)`, in one shot.
    ///
    /// Sweeps a whole namespace at once — e.g. all `worktree:{session_id}:*`
    /// roots a session leaked when a `WorktreeRemove` was missed (the
    /// `SessionEnd` backstop and the daemon root-GC, workstream 30).
    ///
    /// Returns the number of contributor keys removed. `remove_contributor`
    /// returns nothing and callers re-sync unconditionally; the count here
    /// lets a sweep caller skip `sync_roots` when nothing matched (`0`) and
    /// re-sync only when the union actually changed (`> 0`).
    fn remove_contributors_with_prefix(&self, prefix: &str) -> usize {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = inner.contributors.len();
        inner
            .contributors
            .retain(|contributor, _| !contributor.starts_with(prefix));
        let removed = before - inner.contributors.len();
        if removed > 0 {
            inner.reconcile_roots();
        }
        removed
    }

    /// Returns each contributor whose key `starts_with(prefix)`, paired with its
    /// roots.
    ///
    /// `list_roots` inverts to path→sources; this enumerates contributor→roots
    /// without inverting, so a caller can inspect each contributor's own root set
    /// (e.g. the daemon root-GC reading every `worktree:*` contributor's path to
    /// test it against the filesystem). Semantics-free: the tracker stays a pure
    /// data structure with no knowledge of `worktree:` or the filesystem.
    fn contributors_with_prefix(&self, prefix: &str) -> Vec<(String, Vec<PathBuf>)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contributors
            .iter()
            .filter(|(contributor, _)| contributor.starts_with(prefix))
            .map(|(contributor, roots)| (contributor.clone(), roots.iter().cloned().collect()))
            .collect()
    }

    /// Removes a single root from a contributor's set.
    ///
    /// Returns `true` if the root was present and removed, `false` if
    /// the contributor or root was not found.
    #[allow(
        clippy::option_if_let_else,
        reason = "map_or's closure would re-borrow inner.contributors while the get_mut borrow is live"
    )]
    fn remove_root(&self, contributor: &str, root: &Path) -> bool {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = if let Some(roots) = inner.contributors.get_mut(contributor) {
            let removed = roots.remove(root);
            if roots.is_empty() {
                inner.contributors.remove(contributor);
            }
            removed
        } else {
            false
        };
        if removed {
            inner.reconcile_roots();
        }
        removed
    }

    /// Returns the union of all contributors' root sets (path-only view).
    fn global_roots(&self) -> Vec<PathBuf> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roots
            .keys()
            .cloned()
            .collect()
    }

    /// Returns the canonical config-complete [`Root`]s — the rich view the
    /// daemon pushes down to `Session::sync_roots`/`LspClientManager`.
    fn global_roots_rich(&self) -> Vec<Arc<Root>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roots
            .values()
            .cloned()
            .collect()
    }

    /// Returns all roots with their contributor sources.
    ///
    /// Each entry is `(path, sources)` where `sources` is a sorted list
    /// of contributor keys (e.g., `["hook", "mcp:3"]`).
    fn list_roots(&self) -> Vec<(PathBuf, Vec<String>)> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Invert: root → list of contributors.
        let mut root_sources: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for (contributor, roots) in &inner.contributors {
            for root in roots {
                root_sources
                    .entry(root.clone())
                    .or_default()
                    .push(contributor.clone());
            }
        }
        drop(inner);

        let mut result: Vec<(PathBuf, Vec<String>)> = root_sources.into_iter().collect();
        result.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (_, sources) in &mut result {
            sources.sort();
        }
        result
    }

    /// Returns the number of contributors that include the given root.
    #[cfg(test)]
    fn refcount(&self, root: &Path) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contributors
            .values()
            .filter(|roots| roots.contains(root))
            .count()
    }
}

/// Parses the contributing `session_id` out of a `worktree:{session_id}:{path}`
/// contributor key.
///
/// The reaper task has no session span (it runs in the background watcher), so it
/// recovers the session id from the contributor to scope its reap log under the
/// same firehose shard as the mount (which logs inside the session-scoped hook
/// handler). Returns `None` if the key is not a well-formed `worktree:` key.
fn worktree_contributor_session_id(contributor: &str) -> Option<&str> {
    let rest = contributor.strip_prefix("worktree:")?;
    let (session_id, _path) = rest.split_once(':')?;
    (!session_id.is_empty()).then_some(session_id)
}

/// Reaps every `worktree:*` contributor whose worktree directory is gone on
/// disk (a missed `WorktreeRemove`). Returns the removed contributor keys so the
/// caller can re-sync + log. Path-existence is a direct, correlation-free,
/// session-free, within-session signal (ticket 03).
///
/// A `worktree:{session_id}:{path}` key holds exactly one path; the contributor
/// is reaped when none of its paths exist (`!path.exists()`). Pure filesystem +
/// [`RootTracker`]: no async, and no `sync_roots` here — the periodic loop owns
/// the (single) re-sync once it has the removed set.
#[cfg(unix)]
fn reap_missing_worktree_roots(tracker: &RootTracker) -> Vec<String> {
    let mut removed = Vec::new();
    for (key, roots) in tracker.contributors_with_prefix("worktree:") {
        if !roots.iter().any(|path| path.exists()) {
            tracker.remove_contributor(&key);
            removed.push(key);
        }
    }
    removed
}

// ── Ephemeral, activity-mounted roots (ephemeral-roots ticket 02) ───────────

/// Contributor-key prefix for activity-mounted ephemeral roots.
///
/// A `RootTracker` contributor keyed `ephemeral:{canonical root path}` (decision
/// 021's namespace discipline — a dedicated class beside `mcp:{fd}` / `hook` /
/// `worktree:*`, keyed on the canonical root path). A CLI query (`grep` / `glob`
/// / `diagnostics`) touching a path outside every mounted root mounts the
/// enclosing project root under this key; the idle-expiry reaper tears it down.
#[cfg(unix)]
const EPHEMERAL_CONTRIBUTOR_PREFIX: &str = "ephemeral:";

/// How long an ephemeral root survives without a refreshing activity before the
/// reaper tears it down. In the ticket's 5–10 minute band; the reaper sweep
/// interval adds at most one [`EPHEMERAL_ROOT_SWEEP_INTERVAL`] of slack, keeping
/// the worst case under 10 minutes.
#[cfg(unix)]
const EPHEMERAL_ROOT_IDLE_TIMEOUT: Duration = Duration::from_mins(7);

/// How often the idle-expiry reaper wakes to sweep inactive ephemeral roots.
#[cfg(unix)]
const EPHEMERAL_ROOT_SWEEP_INTERVAL: Duration = Duration::from_mins(1);

/// Builds the `ephemeral:{canonical root path}` contributor key for a root.
#[cfg(unix)]
fn ephemeral_contributor(root: &Path) -> String {
    format!("{EPHEMERAL_CONTRIBUTOR_PREFIX}{}", root.display())
}

/// Per-root idle clock for activity-mounted ephemeral roots.
///
/// Holds the last-activity [`Instant`] of every ephemeral root, keyed by
/// canonical path. Every qualifying activity (search, outline, diagnostics, edit
/// tracking) [`touch`](Self::touch)es the covering root; the idle reaper reads
/// [`expired`](Self::expired) to decide teardown. The clock is the *only*
/// release signal — an activity-created mount has no MCP heartbeat to pin on
/// (DESIGN.md), so inactivity is the correct expiry.
///
/// `Instant`-based and injectable: the reaper's `now`/`idle` are parameters, so
/// tests drive expiry deterministically (a stale `Instant::now() - Duration`)
/// with no wall-clock sleep (zero-flake doctrine).
#[cfg(unix)]
#[derive(Clone)]
struct EphemeralMounts {
    inner: Arc<std::sync::Mutex<HashMap<PathBuf, Instant>>>,
}

#[cfg(unix)]
impl EphemeralMounts {
    fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Records `now` as the last-activity time for `root` (mount or refresh).
    fn touch(&self, root: &Path, now: Instant) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.to_path_buf(), now);
    }

    /// Refreshes every ephemeral root that encloses `path` (an ancestor-or-equal
    /// of it). `path` should already be canonicalized so it lines up with the
    /// canonical root keys. A qualifying activity on a file under an ephemeral
    /// root keeps that root alive.
    fn touch_covering(&self, path: &Path, now: Instant) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (root, last) in inner.iter_mut() {
            if path.starts_with(root) {
                *last = now;
            }
        }
    }

    /// Drops a root's clock entry (on idle expiry or upgrade-to-pinned).
    fn remove(&self, root: &Path) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(root);
    }

    /// Seconds until the root at `path` expires on idle, given `now` and the
    /// idle `timeout`. `None` when the root carries no idle clock (not an
    /// activity-mounted ephemeral root). Saturating, so an already-past deadline
    /// reads `0` rather than underflowing.
    fn idle_remaining(&self, path: &Path, now: Instant, timeout: Duration) -> Option<Duration> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .map(|last| timeout.saturating_sub(now.saturating_duration_since(*last)))
    }

    /// Returns the roots whose last activity is at least `idle` before `now`.
    ///
    /// `saturating_duration_since` guards against a `last` in the future (clock
    /// skew is impossible for a monotonic `Instant`, but the saturating form is
    /// panic-free regardless).
    fn expired(&self, now: Instant, idle: Duration) -> Vec<PathBuf> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, last)| now.saturating_duration_since(**last) >= idle)
            .map(|(root, _)| root.clone())
            .collect()
    }

    /// The set of roots that currently carry an idle clock (test-only).
    #[cfg(test)]
    fn roots(&self) -> Vec<PathBuf> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

/// Daemon-side first-sighting ledger for the Antigravity `PreInvocation`
/// teaching injection (teaching-surface ticket 03).
///
/// Holds every `conversationId` (Antigravity's `session_id`) whose
/// `PreInvocation` hook has already been told to inject the teaching payload.
/// [`see`](Self::see) atomically records a conversation and reports whether this
/// was its first sighting, so the persisted `injectSteps` `userMessage` is
/// delivered exactly once per conversation — independent of `invocationNum`
/// semantics on resume (a resumed conversation restores its transcript, so the
/// daemon's memory, not the counter, is the dedup authority).
///
/// The ledger lives for the daemon's lifetime. A daemon restart forgets it,
/// which at worst re-teaches an in-flight conversation once — a bounded,
/// acceptable re-stamp, analogous to the Claude re-stamp on a context
/// discontinuity. Antigravity sends no `session-end`, so entries are never
/// evicted mid-conversation; memory is one short string per conversation.
#[cfg(unix)]
#[derive(Clone, Default)]
struct FirstSightings {
    inner: Arc<std::sync::Mutex<HashSet<String>>>,
}

#[cfg(unix)]
impl FirstSightings {
    fn new() -> Self {
        Self::default()
    }

    /// Records `conversation_id` and returns `true` iff it was **not** already
    /// present — i.e. this is its first sighting and the teaching payload should
    /// be injected now. Every later call for the same id returns `false`.
    fn see(&self, conversation_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(conversation_id.to_string())
    }
}

/// Decides whether a touched path warrants mounting an enclosing ephemeral root.
///
/// Returns the canonical enclosing project root to mount, or `None` when no
/// mount is warranted. `Some(root)` iff:
///
/// - the touched path is **not** already inside any tracked root (equal to or
///   under one — the "outside every mounted root" test, which also rejects a
///   path under a mounted sub-root), and
/// - an enclosing project root is detectable by walking repository markers
///   (`.git`/`.svn`/`.hg`/`.jj`) up from the path
///   ([`crate::companions::enclosing_worktree_root`]), and
/// - that root is not itself already tracked.
///
/// `canonical_touched` should be canonicalized by the caller when the path
/// exists so the comparison lines up with the tracker's canonical roots; a glob
/// pattern or not-yet-existing path (which cannot canonicalize) still resolves
/// its enclosing repository root by lexical ancestor walk. Scope guard: only the single
/// enclosing root is returned — never a sibling — and companion templating is
/// never applied to it.
#[cfg(unix)]
fn ephemeral_root_to_mount(
    canonical_touched: &Path,
    tracked: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    // Already inside a tracked root → covered, no ephemeral mount.
    if tracked.iter().any(|r| canonical_touched.starts_with(r)) {
        return None;
    }
    let root = crate::companions::enclosing_worktree_root(canonical_touched)?;
    let root = root.canonicalize().unwrap_or(root);
    // Belt-and-suspenders vs the check above (a canonicalization mismatch): the
    // enclosing root is already a tracked root.
    if tracked.contains(&root) {
        return None;
    }
    Some(root)
}

/// Reaps every ephemeral root idle beyond `idle` as of `now`, returning the
/// reaped root paths so the caller can re-sync + log.
///
/// Pure [`RootTracker`] + [`EphemeralMounts`] mutation — no async, no
/// `sync_roots` (the reaper loop owns the single re-sync once it has the reaped
/// set), mirroring [`reap_missing_worktree_roots`]. `now`/`idle` are injected so
/// tests drive expiry with no wall-clock wait. A reaped root with outstanding
/// debt is coverage loss, not a wedge (decision 027): the removed server means
/// the file degrades to uncovered, and a later `catenary diagnostics` on it
/// re-mounts (activity) or degrades honestly — the gate never strands.
#[cfg(unix)]
fn reap_idle_ephemeral_roots(
    tracker: &RootTracker,
    mounts: &EphemeralMounts,
    now: Instant,
    idle: Duration,
) -> Vec<PathBuf> {
    let expired = mounts.expired(now, idle);
    for root in &expired {
        tracker.remove_contributor(&ephemeral_contributor(root));
        mounts.remove(root);
    }
    expired
}

// ── Worktree-class roots: registry, idle clock, blocked-on-permission (misc 150) ──

/// How long a mounted worktree root survives without a refreshing activity
/// before the idle reaper unmounts it.
///
/// Longer than [`EPHEMERAL_ROOT_IDLE_TIMEOUT`] — a worktree subagent is a
/// heavier, longer-lived context whose servers are worth keeping warm across
/// brief lulls. A blocked-on-permission root is exempt from this expiry.
#[cfg(unix)]
const WORKTREE_ROOT_IDLE_TIMEOUT: Duration = Duration::from_mins(30);

/// How often the worktree idle-expiry reaper wakes to sweep inactive worktree
/// roots. Coarse relative to the 30-minute timeout — the extra slack is at most
/// one sweep interval.
#[cfg(unix)]
const WORKTREE_ROOT_IDLE_SWEEP_INTERVAL: Duration = Duration::from_mins(5);

/// One worktree root's idle clock and blocked-on-permission flag.
#[cfg(unix)]
struct WorktreeClock {
    /// The tracked root path (for `touch_covering`'s prefix test and logging).
    root: PathBuf,
    /// Last qualifying activity under the root.
    last: Instant,
    /// Blocked-on-permission: idle expiry is suspended while set.
    blocked: bool,
}

/// Per-root idle + blocked state for mounted worktree-class roots, keyed by
/// contributor (misc 150).
///
/// The worktree analogue of [`EphemeralMounts`], but keyed by the **contributor**
/// rather than the path: a worktree contributor may be identity-shaped
/// (`worktree:{session}:{agent_id}`), which is not path-derivable, so the reaper
/// must carry the key it will `remove_contributor`. Each entry also tracks a
/// blocked-on-permission flag — a subagent parked at a permission prompt for
/// hours is *blocked*, not orphaned, so it is exempt from idle expiry until the
/// flag clears (an identity event or qualifying activity).
#[cfg(unix)]
#[derive(Clone)]
struct WorktreeMounts {
    inner: Arc<std::sync::Mutex<HashMap<String, WorktreeClock>>>,
}

#[cfg(unix)]
impl WorktreeMounts {
    fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Start (or refresh) a mounted worktree root's idle clock. Mounting always
    /// clears any stale blocked flag.
    fn track(&self, contributor: &str, root: &Path, now: Instant) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                contributor.to_string(),
                WorktreeClock {
                    root: root.to_path_buf(),
                    last: now,
                    blocked: false,
                },
            );
    }

    /// Refresh — and unblock — every worktree root that encloses `path`.
    ///
    /// Qualifying activity under a root both keeps it alive and clears its
    /// blocked-on-permission suspension (the agent resumed work). `path` should
    /// already be canonicalized so it lines up with the canonical root keys.
    fn touch_covering(&self, path: &Path, now: Instant) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for clock in inner.values_mut() {
            if path.starts_with(&clock.root) {
                clock.last = now;
                clock.blocked = false;
            }
        }
    }

    /// Mark a single worktree root blocked (idle expiry suspended). Returns
    /// whether a matching root was found.
    fn mark_blocked(&self, contributor: &str) -> bool {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(clock) = inner.get_mut(contributor) {
            clock.blocked = true;
            true
        } else {
            false
        }
    }

    /// Mark every worktree root of a session blocked — the coarse fallback when
    /// a permission payload carries no agent identity. Returns the count marked.
    fn mark_blocked_session(&self, session_id: &str) -> usize {
        let prefix = format!("worktree:{session_id}:");
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut marked = 0;
        for (contributor, clock) in inner.iter_mut() {
            if contributor.starts_with(&prefix) {
                clock.blocked = true;
                marked += 1;
            }
        }
        drop(inner);
        marked
    }

    /// Clear a worktree root's blocked flag (an identity event resumed it).
    fn clear_blocked(&self, contributor: &str) {
        if let Some(clock) = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(contributor)
        {
            clock.blocked = false;
        }
    }

    /// Drop a root's clock entry (on reap).
    fn remove(&self, contributor: &str) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(contributor);
    }

    /// Drop every clock entry whose contributor starts with `prefix` (the
    /// `SessionEnd` sweep).
    fn remove_prefix(&self, prefix: &str) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|contributor, _| !contributor.starts_with(prefix));
    }

    /// The `(contributor, root)` of every **non-blocked** root idle at least
    /// `idle` before `now`.
    fn expired(&self, now: Instant, idle: Duration) -> Vec<(String, PathBuf)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, clock)| {
                !clock.blocked && now.saturating_duration_since(clock.last) >= idle
            })
            .map(|(contributor, clock)| (contributor.clone(), clock.root.clone()))
            .collect()
    }

    /// Every mounted worktree root path with its blocked flag (misc 151 —
    /// the `catenary worktree ls` root-state column).
    fn mounted_roots(&self) -> Vec<(PathBuf, bool)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|clock| (clock.root.clone(), clock.blocked))
            .collect()
    }

    /// Whether a contributor's root is currently blocked (test-only).
    #[cfg(test)]
    fn is_blocked(&self, contributor: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(contributor)
            .is_some_and(|clock| clock.blocked)
    }

    /// Whether a contributor carries an idle clock (test-only).
    #[cfg(test)]
    fn contains(&self, contributor: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(contributor)
    }
}

/// Reaps every worktree root idle beyond `idle` as of `now` (blocked roots
/// exempt), returning the reaped `(contributor, root)` pairs so the caller can
/// re-sync + log.
///
/// Pure [`RootTracker`] + [`WorktreeMounts`] (+ watch) mutation — no async, no
/// `sync_roots` (the reaper loop owns the single re-sync), mirroring
/// [`reap_idle_ephemeral_roots`]. **Never** any disk action: a worktree can hold
/// unlanded work, so idle expiry only drops the in-memory root + its language
/// servers; the directory and its sidecar are untouched.
#[cfg(unix)]
fn reap_idle_worktree_roots(
    tracker: &RootTracker,
    mounts: &WorktreeMounts,
    watcher: Option<&crate::worktree_watch::WorktreeWatcher>,
    now: Instant,
    idle: Duration,
) -> Vec<(String, PathBuf)> {
    let expired = mounts.expired(now, idle);
    for (contributor, _) in &expired {
        tracker.remove_contributor(contributor);
        if let Some(watcher) = watcher {
            watcher.unregister(contributor);
        }
        mounts.remove(contributor);
    }
    expired
}

/// In-memory identity→(path, metadata) registry for Catenary-created worktrees
/// (misc 150).
///
/// Populated at `worktree-create/log-payload` (the creation forward, upgraded to
/// a registration) and rehydrated at daemon startup by scanning the agents
/// subtree for sidecars, so a daemon restart loses nothing durable. Keyed by
/// canonical worktree path (unique per worktree): rehydration and the
/// `worktree-remove` reverse lookup are then direct, and the `(session, agent)`
/// lookup is a small scan (a rare event). It corroborates the reap and carries
/// the disposal metadata misc 151 will consume.
#[cfg(unix)]
#[derive(Clone)]
struct WorktreeRegistry {
    inner: Arc<std::sync::Mutex<HashMap<PathBuf, crate::worktree_create::WorktreeMeta>>>,
    /// Worktrees already nagged about this daemon lifetime (misc 151 D-2): the
    /// lingering nag fires **once per worktree** (a doorbell, not an alarm clock).
    /// A worktree surviving into a new session gets fresh surfacing from the
    /// `SessionStart` line, not a re-nag.
    nagged: Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
}

#[cfg(unix)]
impl WorktreeRegistry {
    fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
            nagged: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Record that a worktree has been nagged about; returns `true` only the
    /// first time (misc 151 D-2 — once per worktree).
    fn mark_nagged(&self, worktree: &Path) -> bool {
        self.nagged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(worktree.to_path_buf())
    }

    /// Register (or replace) a worktree by its canonical path.
    fn register(&self, meta: crate::worktree_create::WorktreeMeta) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(meta.worktree.clone(), meta);
    }

    /// Rehydrate from a batch of sidecar metas (daemon startup).
    fn rehydrate(&self, metas: Vec<crate::worktree_create::WorktreeMeta>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for meta in metas {
            inner.insert(meta.worktree.clone(), meta);
        }
    }

    /// The registered [`WorktreeMeta`](crate::worktree_create::WorktreeMeta) for a
    /// canonical worktree path (misc 151 disposal — the in-memory record).
    fn get(&self, worktree: &Path) -> Option<crate::worktree_create::WorktreeMeta> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(worktree)
            .cloned()
    }

    /// Every registered worktree of a session (misc 151 — the `SessionEnd` sweep
    /// disposes this session's clean worktrees).
    fn metas_for_session(&self, session_id: &str) -> Vec<crate::worktree_create::WorktreeMeta> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|meta| meta.session_id == session_id)
            .cloned()
            .collect()
    }

    /// Drop a worktree's registration (misc 151 — after a successful disposal, so
    /// the registry does not carry a stale entry).
    fn forget(&self, worktree: &Path) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(worktree);
    }

    /// The registered worktree path for a `(session_id, agent_id)` identity.
    fn path_for_identity(&self, session_id: &str, agent_id: &str) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|meta| {
                meta.session_id == session_id && meta.agent_id.as_deref() == Some(agent_id)
            })
            .map(|meta| meta.worktree.clone())
    }

    /// The number of registered worktrees (test-only).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Resolves a query's path arguments to absolute touched paths.
///
/// Absolute args pass through; relative args (including glob patterns like
/// `src/**/*.rs`) are joined onto `cwd`. When no paths are named, the query's
/// effective target is `cwd` itself, so it is returned as the single touched
/// path (mounting the enclosing root of a bare `grep` run inside an out-of-root
/// checkout). Returns empty when there is nothing to resolve (no paths, no cwd).
#[cfg(unix)]
fn resolve_touched_paths(paths: &[PathBuf], cwd: Option<&Path>) -> Vec<PathBuf> {
    if paths.is_empty() {
        return cwd.map(Path::to_path_buf).into_iter().collect();
    }
    paths
        .iter()
        .map(|p| match cwd {
            Some(base) if p.is_relative() => base.join(p),
            _ => p.clone(),
        })
        .collect()
}

/// Mounts an ephemeral root for every touched path outside every mounted root,
/// and refreshes the idle clock of every ephemeral root the touched paths fall
/// under (ephemeral-roots ticket 02).
///
/// Called by the `grep` / `glob` / `diagnostics` handlers before they execute,
/// so the enriched/diagnosed result is served from the freshly-attached
/// server(s). A new mount adds an `ephemeral:{path}` contributor and re-syncs
/// the union (the same `sync_roots` path root removal rides, so the fresh server
/// spawns exactly as a pinned root's would); the sync happens once, after all
/// paths are processed. Idempotent per path: an already-mounted root only has
/// its clock refreshed. First-touch pays the new server's spawn/index (an
/// accepted slight stall); existing roots are never torn down here, so other
/// roots' work is not blocked. Scope guard: only enclosing roots mount, never
/// siblings, and companion templating is never applied.
#[cfg(unix)]
async fn ensure_ephemeral_mounts(
    ctx: &HookDispatchContext,
    touched: &[PathBuf],
    now: Instant,
    session_id: &str,
) {
    let Some(tracker) = &ctx.root_tracker else {
        return;
    };
    let mounts = &ctx.ephemeral_mounts;
    let mut mounted = false;
    for path in touched {
        // Canonicalize when the path exists so the comparison lines up with the
        // tracker's canonical roots; a glob pattern / not-yet-existing path keeps
        // its resolved spelling (its enclosing `.git` still resolves lexically).
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        // Every qualifying activity refreshes the covering root's idle clock —
        // both the ephemeral clock and the worktree-class clock (misc 150), the
        // latter also clearing any blocked-on-permission flag.
        mounts.touch_covering(&canonical, now);
        ctx.worktree_mounts.touch_covering(&canonical, now);
        let existing: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();
        if let Some(root) = ephemeral_root_to_mount(&canonical, &existing) {
            let contributor = ephemeral_contributor(&root);
            tracker.set_roots(&contributor, vec![root.clone()]);
            mounts.touch(&root, now);
            mounted = true;
            info!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                root = %root.display(),
                contributor = %contributor,
                "mounted ephemeral root on out-of-root activity",
            );
        }
    }
    if mounted {
        if let Err(e) = ctx.primary.sync_roots(tracker.global_roots_rich()).await {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                "root sync after ephemeral mount failed: {e}",
            );
        }
        // The root board changed — flush the snapshot so `state.json` shows the
        // new ephemeral mount promptly (the board is pulled at flush time).
        ctx.primary.touch_snapshot();
    }
}

/// Core daemon component that manages MCP and hook socket connections.
///
/// Binds two Unix domain sockets: one for MCP connections from `catenary
/// bridge` proxies, one for hook connections from `catenary hook` CLI
/// processes. Each MCP connection spawns a per-connection async task
/// with a protocol-only `McpServer` (roots, lifecycle). Hook connections
/// are routed to per-`session_id` [`HookRouter`] instances when a shared
/// [`Session`] is configured (daemon mode), or receive passthrough
/// responses (test mode).
#[cfg(unix)]
pub struct SessionManager {
    mcp_listener: tokio::net::UnixListener,
    ipc_listener: tokio::net::UnixListener,
    mcp_socket_path: PathBuf,
    ipc_socket_path: PathBuf,
    logging: LoggingServer,
    connection_count: Arc<AtomicUsize>,
    /// Monotonic counter for unique MCP connection IDs. Incremented
    /// once per accepted connection; never decremented. Used as the
    /// session key (`mcp:{n}`) to avoid fd-reuse collisions.
    next_connection_id: Arc<AtomicUsize>,
    /// Session-aware hook dispatch context. `None` in tests that don't
    /// exercise hook routing (passthrough mode).
    hook_ctx: Option<HookDispatchContext>,
    /// Shared LSP infrastructure for MCP lifecycle callbacks
    /// (`on_roots_changed`). `None` in transport-only tests.
    lsp: Option<Arc<crate::lsp::LspClientManager>>,
    /// Root tracker for refcount-aware root management across sessions.
    /// `None` in transport-only tests; set by [`Self::with_session`].
    root_tracker: Option<RootTracker>,
    shutdown: CancellationToken,
    disconnect: Arc<tokio::sync::Notify>,
    /// Receiver for worktree-deletion events from the [`crate::worktree_watch`]
    /// watcher, stashed by [`Self::with_session`] and taken once by
    /// [`Self::spawn_worktree_watch_reaper`]. `None` until `with_session` wires
    /// the watcher (or if the OS watcher couldn't be created).
    worktree_watch_rx: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::worktree_watch::WorktreeDeleted>>,
    >,
}

#[cfg(unix)]
impl SessionManager {
    /// Binds the MCP and IPC sockets at the default paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory cannot be created or
    /// either socket cannot be bound (e.g., another daemon is already
    /// running).
    pub fn bind(logging: LoggingServer) -> Result<Self> {
        Self::bind_at(&mcp_socket_path(), &socket_path(), logging)
    }

    /// Creates a `SessionManager` from pre-bound sockets.
    ///
    /// Consumes the [`DaemonSockets`] returned by [`bind_daemon_sockets`],
    /// transferring socket ownership. Used in daemon mode where sockets
    /// are bound before heavy initialization so bridges can connect
    /// immediately. [`SessionManager::drop`] cleans up the socket files.
    #[must_use]
    pub fn from_sockets(sockets: DaemonSockets, logging: LoggingServer) -> Self {
        Self {
            mcp_listener: sockets.mcp_listener,
            ipc_listener: sockets.ipc_listener,
            mcp_socket_path: sockets.mcp_path,
            ipc_socket_path: sockets.ipc_path,
            logging,
            connection_count: Arc::new(AtomicUsize::new(0)),
            next_connection_id: Arc::new(AtomicUsize::new(0)),
            hook_ctx: None,
            lsp: None,
            root_tracker: None,
            shutdown: CancellationToken::new(),
            disconnect: Arc::new(tokio::sync::Notify::new()),
            worktree_watch_rx: std::sync::Mutex::new(None),
        }
    }

    /// Binds the MCP and IPC sockets at explicit paths.
    ///
    /// Used by tests to isolate socket files in tempdirs.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directories cannot be created or
    /// either socket cannot be bound.
    pub fn bind_at(mcp_path: &Path, ipc_path: &Path, logging: LoggingServer) -> Result<Self> {
        if let Some(parent) = mcp_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory: {}", parent.display()))?;
        }
        if let Some(parent) = ipc_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory: {}", parent.display()))?;
        }

        let mcp_listener = tokio::net::UnixListener::bind(mcp_path)
            .with_context(|| format!("bind MCP socket: {}", mcp_path.display()))?;
        let ipc_listener = tokio::net::UnixListener::bind(ipc_path)
            .with_context(|| format!("bind IPC socket: {}", ipc_path.display()))?;

        info!(
            source = Source::DaemonLifecycle.as_str(),
            mcp_path = %mcp_path.display(),
            ipc_path = %ipc_path.display(),
            "daemon started",
        );

        Ok(Self {
            mcp_listener,
            ipc_listener,
            mcp_socket_path: mcp_path.to_path_buf(),
            ipc_socket_path: ipc_path.to_path_buf(),
            logging,
            connection_count: Arc::new(AtomicUsize::new(0)),
            next_connection_id: Arc::new(AtomicUsize::new(0)),
            hook_ctx: None,
            lsp: None,
            root_tracker: None,
            shutdown: CancellationToken::new(),
            disconnect: Arc::new(tokio::sync::Notify::new()),
            worktree_watch_rx: std::sync::Mutex::new(None),
        })
    }

    /// Accepts incoming MCP and IPC connections in a loop.
    ///
    /// Each MCP connection spawns a per-connection async task with a
    /// `McpServer` (protocol-only, no tools). The task runs in a tracing
    /// span tagged with `mcp_fd` for log correlation. IPC connections
    /// are short-lived and handled in spawned tasks with passthrough
    /// responses.
    ///
    /// Returns `Ok(())` when the daemon should shut down. Three triggers:
    /// - Last MCP client disconnected (disconnect notify, count == 0)
    /// - `catenary stop` received on the IPC socket (shutdown token)
    /// - External signal cancelled the shutdown token
    ///
    /// On exit, socket files are removed so new bridges start a fresh
    /// daemon instead of connecting to one that is shutting down.
    ///
    /// # Errors
    ///
    /// Returns an error if either listener encounters a fatal I/O error.
    pub async fn accept_loop(&self) -> Result<()> {
        use std::os::fd::AsRawFd;

        loop {
            tokio::select! {
                result = self.mcp_listener.accept() => {
                    let (stream, _addr) = result.context("accept MCP connection")?;
                    let fd = stream.as_raw_fd();
                    self.handle_mcp_connection(stream, fd);
                }
                result = self.ipc_listener.accept() => {
                    let (stream, _addr) = result.context("accept IPC connection")?;
                    let shutdown = self.shutdown.clone();
                    let connections = Arc::clone(&self.connection_count);
                    if let Some(ctx) = &self.hook_ctx {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_hook_dispatch(stream, ctx, shutdown, connections).await
                            {
                                debug!(
                                    source = Source::DaemonDispatch.as_str(),
                                    "hook connection error: {e}",
                                );
                            }
                        });
                    } else {
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_hook_connection(stream, shutdown, connections).await
                            {
                                debug!(
                                    source = Source::DaemonDispatch.as_str(),
                                    "hook connection error: {e}",
                                );
                            }
                        });
                    }
                }
                () = self.shutdown.cancelled() => {
                    self.remove_sockets();
                    return Ok(());
                }
                () = self.disconnect.notified() => {
                    if self.connection_count.load(Ordering::Acquire) == 0 {
                        info!(
                            source = Source::DaemonLifecycle.as_str(),
                            "last client disconnected",
                        );
                        self.remove_sockets();
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Returns the shutdown token for this daemon.
    ///
    /// Cancel this token to initiate daemon shutdown. The
    /// [`accept_loop`](Self::accept_loop) removes socket files and
    /// returns `Ok(())` when the token is cancelled.
    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Removes socket files so new bridges start a fresh daemon.
    fn remove_sockets(&self) {
        let _ = std::fs::remove_file(&self.mcp_socket_path);
        let _ = std::fs::remove_file(&self.ipc_socket_path);
    }

    /// Spawns a per-connection MCP task.
    ///
    /// Converts the tokio `UnixStream` to a `std::os::unix::net::UnixStream`
    /// (since `McpServer` uses synchronous I/O), clones it for
    /// read/write halves, and runs the MCP message loop in a blocking task.
    /// A [`ConnectionGuard`] decrements the connection count on any exit
    /// path and notifies the accept loop, which checks whether the daemon
    /// should shut down.
    #[allow(clippy::too_many_lines, reason = "sequential connection setup steps")]
    fn handle_mcp_connection(&self, stream: tokio::net::UnixStream, fd: i32) {
        let logging = self.logging.clone();
        let count = Arc::clone(&self.connection_count);
        let disconnect = Arc::clone(&self.disconnect);
        let lsp = self.lsp.clone();
        let primary_session = self.hook_ctx.as_ref().map(|ctx| ctx.primary.clone());
        let root_tracker = self.root_tracker.clone();

        // Per-connection session key. Monotonic counter avoids
        // collisions from fd reuse across the daemon's lifetime. The
        // `mcp:` prefix tags the tracing span for log correlation.
        let conn_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let session_key = format!("mcp:{conn_id}");

        count.fetch_add(1, Ordering::Relaxed);

        tokio::spawn(async move {
            let span = tracing::info_span!(
                "mcp_connection",
                mcp_fd = fd,
                session_id = %session_key,
            );
            let span_for_blocking = span.clone();
            async {
                let _guard = ConnectionGuard { count, disconnect };

                info!(
                    source = Source::DaemonDispatch.as_str(),
                    "MCP connection accepted",
                );

                let std_stream = match stream.into_std() {
                    Ok(s) => {
                        // into_std() returns a non-blocking stream (tokio
                        // default). McpServer uses blocking I/O — switch
                        // to blocking mode before handing off.
                        if let Err(e) = s.set_nonblocking(false) {
                            error!(
                                source = Source::DaemonDispatch.as_str(),
                                "failed to set stream to blocking: {e}",
                            );
                            return;
                        }
                        s
                    }
                    Err(e) => {
                        error!(
                            source = Source::DaemonDispatch.as_str(),
                            "failed to convert socket to std: {e}",
                        );
                        return;
                    }
                };
                let reader = match std_stream.try_clone() {
                    Ok(r) => r,
                    Err(e) => {
                        error!(
                            source = Source::DaemonDispatch.as_str(),
                            "failed to clone socket for reader: {e}",
                        );
                        return;
                    }
                };
                let writer = std_stream;

                // Clone shared state for post-disconnect cleanup
                // (originals move into spawn_blocking).
                let tracker_cleanup = root_tracker.clone();
                let session_cleanup = primary_session.clone();
                let lsp_cleanup = lsp.clone();

                let result = tokio::task::spawn_blocking(move || {
                    let _entered = span_for_blocking.enter();

                    let mut mcp = McpServer::new(logging);

                    // Wire lifecycle callbacks when the shared LSP
                    // infrastructure is available (daemon mode). When a
                    // root tracker is configured, root changes go through
                    // refcounting so multiple sessions can share roots
                    // without clobbering each other.
                    match (root_tracker, lsp, primary_session) {
                        (Some(tracker), Some(_), Some(session)) => {
                            let mcp_key = format!("mcp:{fd}");
                            mcp = mcp.on_roots_changed(Box::new(move |roots| {
                                // Expand the client's declared roots with any
                                // configured companions (workstream 29), then
                                // REPLACE the contributor set — recomputing from
                                // the full declared set on every change tracks
                                // add/remove for free (no provenance bookkeeping).
                                let paths = companion_expanded_roots(&roots, &session.config);
                                tracker.set_roots(&mcp_key, paths);
                                let global = tracker.global_roots_rich();
                                tokio::runtime::Handle::current()
                                    .block_on(session.sync_roots(global))?;
                                Ok(())
                            }));
                        }
                        (None, Some(cm), _) => {
                            mcp = mcp.on_roots_changed(Box::new(move |roots| {
                                // No tracker (transport-only mode): build
                                // config-complete roots inline so the manager
                                // still gets `Root`s, not bare paths.
                                let roots: Vec<Arc<Root>> = parse_root_uris(&roots)
                                    .into_iter()
                                    .map(|p| Arc::new(Root::load(p)))
                                    .collect();
                                tokio::runtime::Handle::current().block_on(cm.sync_roots(roots))?;
                                Ok(())
                            }));
                        }
                        _ => {}
                    }

                    mcp.run(reader, writer)
                })
                .await;

                match result {
                    Ok(Ok(())) => info!(
                        source = Source::DaemonDispatch.as_str(),
                        "MCP connection closed",
                    ),
                    Ok(Err(e)) => error!(
                        source = Source::DaemonDispatch.as_str(),
                        "MCP connection error: {e}",
                    ),
                    Err(e) => error!(
                        source = Source::DaemonDispatch.as_str(),
                        "MCP task panicked: {e}",
                    ),
                }

                // ── Disconnect cleanup ────────────────────────────
                //
                // Remove roots from the tracker and sync the reduced root
                // set to LSP servers.
                if let Some(ref tracker) = tracker_cleanup {
                    let mcp_key = format!("mcp:{fd}");
                    tracker.remove_contributor(&mcp_key);

                    // Sync the reduced root set through the primary
                    // session so both FilesystemManager and PathValidator
                    // are updated.
                    let global = tracker.global_roots_rich();
                    let sync_result = if let Some(ref session) = session_cleanup {
                        session.sync_roots(global).await
                    } else if let Some(ref cm) = lsp_cleanup {
                        cm.sync_roots(global).await.map(|_| ())
                    } else {
                        Ok(())
                    };
                    if let Err(e) = sync_result {
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            "root sync after disconnect failed: {e}",
                        );
                    }
                }
            }
            .instrument(span)
            .await;
        });
    }

    /// Returns the number of active MCP connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::Relaxed)
    }

    /// Returns the MCP socket path this manager is bound to.
    #[must_use]
    pub fn mcp_path(&self) -> &Path {
        &self.mcp_socket_path
    }

    /// Returns the IPC socket path this manager is bound to.
    #[must_use]
    pub fn ipc_path(&self) -> &Path {
        &self.ipc_socket_path
    }

    /// Enables session-aware hook dispatch.
    ///
    /// Once set, hook connections create per-`session_id` [`Session`]
    /// instances (via [`Session::new_for_daemon`]) with independent
    /// per-session state. Heavy resources are shared from the primary
    /// session. Without this, hooks receive passthrough responses (test
    /// mode).
    #[must_use]
    pub fn with_session(mut self, session: Arc<Session>) -> Self {
        self.lsp = Some(session.lsp_client_manager().clone());
        let root_tracker = RootTracker::new();
        self.root_tracker = Some(root_tracker.clone());

        let sessions = Arc::new(std::sync::Mutex::new(HashMap::new()));
        // Shared with the root board so `state.json` can surface each ephemeral
        // root's idle-remaining figure; the hook context holds the same handle.
        let ephemeral_mounts = EphemeralMounts::new();
        // Shared with the session board so `state.json` carries each session's
        // live subagents; the hook context records/prunes into the same handle.
        let subagents = SubagentRegistry::new();
        // Wire the live session board + root board onto the daemon snapshot so
        // `state.json` carries the rich session board (observability ticket 05)
        // and the daemon-level tracked-root board with full contributor classes
        // and ephemeral idle-remaining (ephemeral-roots ticket 02 / tui-rework).
        // The writer pulls both at each flush; `None` outside daemon mode.
        if let Some(snapshot) = &session.snapshot {
            snapshot.set_session_board(Arc::new(SessionBoardImpl {
                sessions: sessions.clone(),
                subagents: subagents.clone(),
            }));
            snapshot.set_root_board(Arc::new(RootBoardImpl {
                tracker: root_tracker.clone(),
                ephemeral_mounts: ephemeral_mounts.clone(),
            }));
        }

        // Bounded worktree-deletion watch (ticket 05): the prompt teardown
        // trigger for `worktree:*` roots, since `git worktree remove` never fires
        // `WorktreeRemove`. A failure to create the OS watcher is non-fatal — the
        // hourly GC remains the backstop — so we degrade to `None` and log.
        let worktree_watcher = match crate::worktree_watch::WorktreeWatcher::new() {
            Ok((watcher, rx)) => {
                *self
                    .worktree_watch_rx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);
                Some(watcher)
            }
            Err(e) => {
                warn!(
                    source = Source::DaemonLifecycle.as_str(),
                    "worktree deletion watcher unavailable (GC remains the backstop): {e}",
                );
                None
            }
        };

        // Rehydrate the worktree registry from sidecars under the agents subtree
        // (misc 150): a daemon restart must lose nothing durable. Cheap synchronous
        // scan at wiring time — one readdir per session dir, before the reapers spawn.
        let worktree_registry = WorktreeRegistry::new();
        worktree_registry.rehydrate(crate::worktree_create::scan_sidecars(
            &crate::paths::agents_worktrees_dir(),
        ));

        self.hook_ctx = Some(HookDispatchContext {
            sessions,
            search_limiter: SearchLimiter::with_default_permits(),
            primary: session,
            _logging: self.logging.clone(),
            root_tracker: Some(root_tracker),
            editing_guardrail: Arc::new(EditingGuardrail::new()),
            handoff: KeyedHandoff::new(),
            worktree_watcher,
            ephemeral_mounts,
            first_sightings: FirstSightings::new(),
            worktree_registry,
            worktree_mounts: WorktreeMounts::new(),
            subagents,
        });
        self
    }

    /// Spawns the periodic worktree-root GC — the crash-safe leak backstop for a
    /// missed `WorktreeRemove`.
    ///
    /// Every [`WORKTREE_ROOT_GC_INTERVAL`] it reaps every `worktree:*`
    /// contributor whose worktree directory is gone on disk (path-existence:
    /// direct, correlation-free, session-free, within-session — *not*
    /// MCP-disconnect, which has no host-session correlation) and re-syncs the
    /// reduced union when anything was removed (ticket 03). A worktree whose dir
    /// *lingers* after a crash-before-git-cleanup while its session is dead is an
    /// accepted residual, bounded by daemon restart: there is no clean
    /// per-session "dead" signal (correlation is structurally unavailable without
    /// MCP tools, which are themselves precluded by cross-host hook support), and
    /// we deliberately do not add a staleness heuristic.
    ///
    /// A detached background task on the provided runtime handle, mirroring the
    /// firehose staleness reaper: it consumes the immediate first tick, then runs
    /// until daemon exit (no `CancellationToken` — the runtime is dropped on
    /// shutdown). No-op unless [`Self::with_session`] has wired the tracker and
    /// primary session (daemon mode); test/transport-only managers skip it.
    pub fn spawn_worktree_root_gc(&self, rt: &tokio::runtime::Handle) {
        let (Some(tracker), Some(ctx)) = (&self.root_tracker, &self.hook_ctx) else {
            return;
        };
        // `RootTracker` is Arc-backed (Clone) and `primary` is an `Arc<Session>`;
        // both clones share the live state the request handlers mutate.
        let tracker = tracker.clone();
        let session = ctx.primary.clone();
        let watcher = ctx.worktree_watcher.clone();
        let mounts = ctx.worktree_mounts.clone();
        rt.spawn(async move {
            let mut ticker = tokio::time::interval(WORKTREE_ROOT_GC_INTERVAL);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                let removed = reap_missing_worktree_roots(&tracker);
                if !removed.is_empty() {
                    // Drop any in-memory watches + idle clocks for the reaped
                    // contributors so neither outlives the root (idempotent).
                    for key in &removed {
                        if let Some(watcher) = &watcher {
                            watcher.unregister(key);
                        }
                        mounts.remove(key);
                    }
                    // Same sync call the request handlers use (`ctx.primary` is
                    // this `session`): re-sync the (now smaller) union once.
                    if let Err(e) = session.sync_roots(tracker.global_roots_rich()).await {
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            "root sync after worktree-root GC failed: {e}",
                        );
                    }
                    info!(
                        source = Source::DaemonDispatch.as_str(),
                        count = removed.len(),
                        "reaped leaked worktree roots whose dir is gone",
                    );
                }
            }
        });
    }

    /// Spawns the idle-expiry reaper for activity-mounted ephemeral roots
    /// (ephemeral-roots ticket 02).
    ///
    /// Every [`EPHEMERAL_ROOT_SWEEP_INTERVAL`] it reaps every `ephemeral:*`
    /// contributor idle beyond [`EPHEMERAL_ROOT_IDLE_TIMEOUT`]
    /// ([`reap_idle_ephemeral_roots`]) and, when anything was reaped, re-syncs
    /// the reduced union once — the same `sync_roots` path a pinned root's
    /// removal rides, so the ephemeral server shuts down cleanly (`shutdown` /
    /// `exit`, no leaked server) exactly as the worktree lifecycle does. Each
    /// expiry emits an `info!` firehose event (no user-notification noise).
    ///
    /// A reaped root with outstanding debt is coverage loss, not a wedge
    /// (decision 027) — the idle clock is refreshed by every qualifying activity,
    /// so a root under active diagnosis never expires mid-run; only a genuinely
    /// idle root does, and its debt degrades honestly on the next run.
    ///
    /// A detached background task mirroring [`Self::spawn_worktree_root_gc`]:
    /// consumes the immediate first tick, then runs until daemon exit. No-op
    /// unless [`Self::with_session`] wired the tracker + primary session.
    pub fn spawn_ephemeral_root_reaper(&self, rt: &tokio::runtime::Handle) {
        let (Some(tracker), Some(ctx)) = (&self.root_tracker, &self.hook_ctx) else {
            return;
        };
        let tracker = tracker.clone();
        let session = ctx.primary.clone();
        let mounts = ctx.ephemeral_mounts.clone();
        rt.spawn(async move {
            let mut ticker = tokio::time::interval(EPHEMERAL_ROOT_SWEEP_INTERVAL);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                let expired = reap_idle_ephemeral_roots(
                    &tracker,
                    &mounts,
                    Instant::now(),
                    EPHEMERAL_ROOT_IDLE_TIMEOUT,
                );
                if !expired.is_empty() {
                    // Same sync the request handlers use: re-sync the (now
                    // smaller) union once, shutting down the reaped roots' servers.
                    if let Err(e) = session.sync_roots(tracker.global_roots_rich()).await {
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            "root sync after ephemeral-root expiry failed: {e}",
                        );
                    }
                    for root in &expired {
                        info!(
                            source = Source::DaemonDispatch.as_str(),
                            root = %root.display(),
                            "expired idle ephemeral root",
                        );
                    }
                    // The root board changed — flush the snapshot so the
                    // expired mount leaves `state.json` promptly.
                    session.touch_snapshot();
                }
            }
        });
    }

    /// Spawns the idle-expiry reaper for mounted worktree-class roots (misc 150).
    ///
    /// Every [`WORKTREE_ROOT_IDLE_SWEEP_INTERVAL`] it unmounts every worktree root
    /// idle beyond [`WORKTREE_ROOT_IDLE_TIMEOUT`] and **not** blocked-on-permission
    /// ([`reap_idle_worktree_roots`]): `remove_contributor` + watch `unregister`,
    /// then one re-sync of the reduced union — the same teardown the
    /// `WorktreeRemove`/`SubagentStop` paths run, so the worktree's servers shut
    /// down cleanly. **Never** any disk action: a worktree can hold unlanded work,
    /// so idle expiry reclaims only RAM. The idle clock is refreshed by every
    /// qualifying activity under the root, so an actively used worktree never
    /// expires mid-work; a blocked root is exempt entirely. Each expiry emits an
    /// `info!` firehose event (no user-notification noise).
    ///
    /// A detached background task mirroring [`Self::spawn_ephemeral_root_reaper`]:
    /// consumes the immediate first tick, then runs until daemon exit. No-op
    /// unless [`Self::with_session`] wired the tracker + primary session.
    pub fn spawn_worktree_root_idle_reaper(&self, rt: &tokio::runtime::Handle) {
        let (Some(tracker), Some(ctx)) = (&self.root_tracker, &self.hook_ctx) else {
            return;
        };
        let tracker = tracker.clone();
        let session = ctx.primary.clone();
        let mounts = ctx.worktree_mounts.clone();
        let watcher = ctx.worktree_watcher.clone();
        rt.spawn(async move {
            let mut ticker = tokio::time::interval(WORKTREE_ROOT_IDLE_SWEEP_INTERVAL);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                let expired = reap_idle_worktree_roots(
                    &tracker,
                    &mounts,
                    watcher.as_ref(),
                    Instant::now(),
                    WORKTREE_ROOT_IDLE_TIMEOUT,
                );
                if !expired.is_empty() {
                    // Same sync the request handlers use: re-sync the (now
                    // smaller) union once, shutting down the reaped roots' servers.
                    if let Err(e) = session.sync_roots(tracker.global_roots_rich()).await {
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            "root sync after worktree idle expiry failed: {e}",
                        );
                    }
                    for (contributor, root) in &expired {
                        info!(
                            source = Source::DaemonDispatch.as_str(),
                            session_id = worktree_contributor_session_id(contributor).unwrap_or(""),
                            root = %root.display(),
                            contributor = %contributor,
                            "reaped idle worktree root",
                        );
                    }
                    // The root board changed — flush the snapshot promptly.
                    session.touch_snapshot();
                }
            }
        });
    }

    /// Spawns the worktree-deletion reaper — the prompt teardown trigger for
    /// `worktree:*` roots (ticket 05).
    ///
    /// Drains the channel the [`crate::worktree_watch::WorktreeWatcher`] feeds from
    /// its [`notify`] callback. Each [`crate::worktree_watch::WorktreeDeleted`] is
    /// the deletion of a watched worktree dir — for git subagents the host runs
    /// `git worktree remove` itself and fires no `WorktreeRemove` hook, so this is
    /// the only prompt signal. On each event it runs the SAME reap the
    /// `WorktreeRemove` handler and the GC run — `remove_contributor` +
    /// `sync_roots` — then drops the now-dead watch. Reaping is idempotent: a
    /// double-reap from the watch, the GC, and the `SessionEnd` sweep is a
    /// harmless no-op (`remove_contributor` of an absent key changes nothing, and
    /// the re-sync is to the same union).
    ///
    /// A detached background task on the provided runtime handle, mirroring
    /// [`Self::spawn_worktree_root_gc`]; it exits when the channel closes (daemon
    /// shutdown drops the watcher). No-op unless [`Self::with_session`] wired the
    /// watcher and the channel (daemon mode); test/transport-only managers skip it.
    pub fn spawn_worktree_watch_reaper(&self, rt: &tokio::runtime::Handle) {
        let Some(mut rx) = self
            .worktree_watch_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let (Some(tracker), Some(ctx)) = (&self.root_tracker, &self.hook_ctx) else {
            return;
        };
        let tracker = tracker.clone();
        let session = ctx.primary.clone();
        let watcher = ctx.worktree_watcher.clone();
        let mounts = ctx.worktree_mounts.clone();
        rt.spawn(async move {
            while let Some(event) = rx.recv().await {
                let contributor = event.contributor;
                // Drop the watch first so a coalesced burst of delete events for
                // the same worktree doesn't re-reap; idempotent either way.
                if let Some(watcher) = &watcher {
                    watcher.unregister(&contributor);
                }
                // Drop the worktree idle clock too so it never outlives the root.
                mounts.remove(&contributor);
                tracker.remove_contributor(&contributor);
                if let Err(e) = session.sync_roots(tracker.global_roots_rich()).await {
                    debug!(
                        source = Source::DaemonDispatch.as_str(),
                        "root sync after worktree-deletion reap failed: {e}",
                    );
                }
                // Scope the reap log under the contributing session — the same
                // firehose shard as the mount (which logs inside the
                // session-scoped hook handler) — so a worktree's full lifecycle
                // co-locates. The reaper has no session span, so recover the id
                // from the contributor; `session_id = ""` falls through to the
                // daemon scope for a malformed key (no behavior change).
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = worktree_contributor_session_id(&contributor).unwrap_or(""),
                    contributor = %contributor,
                    "reaped worktree root on dir deletion",
                );
            }
        });
    }

    /// Spawns the external signed-registry refresh task (tui-rework 08).
    ///
    /// Resolves the registry ([`crate::registry::refresh_once`]) once **on daemon
    /// start**, then — when the registry is enabled — again on the slow
    /// [`crate::registry::DEFAULT_REFRESH_INTERVAL`] (hours-class) cadence. Each
    /// resolution logs its provenance and any degradation findings
    /// ([`crate::registry::log_resolution`]): a bad signature or a stale/failed
    /// refresh becomes a `warn` that reaches the user-notification surface and the
    /// firehose. The shipped default is seed-only (no URL), so the first
    /// resolution is a network-free no-op and the loop exits immediately — nothing
    /// happens for users until the maintainer turns the registry on.
    ///
    /// A fully detached task mirroring the sibling spawn_* reapers; the config is
    /// read independently in a blocking sub-task so a slow config load never
    /// stalls the runtime.
    #[allow(
        clippy::unused_self,
        reason = "mirrors the sibling spawn_* reapers' &self signature; the task is \
                  detached and reads config independently"
    )]
    pub fn spawn_registry_refresh(&self, rt: &tokio::runtime::Handle) {
        rt.spawn(async move {
            loop {
                let joined = tokio::task::spawn_blocking(|| {
                    let cfg = crate::config::Config::load()
                        .ok()
                        .and_then(|c| c.registry)
                        .unwrap_or_default();
                    let resolved = crate::registry::refresh_once(&cfg);
                    let enabled = cfg.effective_url().is_some();
                    (resolved, enabled)
                })
                .await;
                let Ok((resolved, enabled)) = joined else {
                    // The blocking sub-task panicked (unexpected) — stop the loop.
                    break;
                };
                crate::registry::log_resolution(&resolved);
                if !enabled {
                    // Seed-only (the shipped default): nothing to refresh on a
                    // timer, so the task retires after the startup resolution.
                    break;
                }
                tokio::time::sleep(crate::registry::DEFAULT_REFRESH_INTERVAL).await;
            }
        });
    }

    /// Returns the number of active sessions in the registry.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.hook_ctx.as_ref().map_or(0, |ctx| {
            ctx.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        })
    }
}

/// RAII guard that decrements the connection count on drop.
///
/// Always notifies the accept loop after decrementing. The accept loop
/// checks the count atomically and decides whether to shut down — this
/// keeps the shutdown decision synchronous on the accept loop's task,
/// eliminating the race between a new connection arriving and the
/// shutdown firing.
#[cfg(unix)]
struct ConnectionGuard {
    count: Arc<AtomicUsize>,
    disconnect: Arc<tokio::sync::Notify>,
}

#[cfg(unix)]
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
        self.disconnect.notify_one();
    }
}

/// Extracts canonical file paths from MCP root URIs.
///
/// Filters out non-`file://` URIs and roots that fail to canonicalize.
#[cfg(unix)]
fn parse_root_uris(roots: &[crate::mcp::Root]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| {
            root.uri.strip_prefix("file://").and_then(|p| {
                let path = PathBuf::from(p);
                match path.canonicalize() {
                    Ok(canonical) => Some(canonical),
                    Err(e) => {
                        warn!(
                            source = Source::ConfigValidation.as_str(),
                            "Skipping root {p}: {e}",
                        );
                        None
                    }
                }
            })
        })
        .collect()
}

/// Expands an MCP client's declared roots with any configured companions.
///
/// This is the body of the `mcp:{fd}` `on_roots_changed` callback (workstream
/// 29), factored out so it can be tested without a live socket: parse the
/// client's root URIs, then — when `[roots.companions]` is configured — union in
/// each root's derived companion via [`expand_companions`]. With no rules
/// configured it is exactly [`parse_root_uris`].
#[cfg(unix)]
fn companion_expanded_roots(
    roots: &[crate::mcp::Root],
    config: &crate::config::Config,
) -> Vec<PathBuf> {
    let declared = parse_root_uris(roots);
    match config.companion_rules() {
        Some(rules) => expand_companions(declared, rules),
        None => declared,
    }
}

/// Handles a single hook connection.
///
/// Reads the JSON request, logs the method for visibility, and sends an
/// empty response (which means "allow" in the hook protocol). Recognizes
/// the `"tool/shutdown"` method from `catenary stop` and cancels the daemon
/// shutdown token. The shutdown ack reports the live MCP connection count so
/// `catenary stop` can warn about sessions that will lose tooling. Used when
/// no shared session is configured (test mode).
#[cfg(unix)]
async fn handle_hook_connection(
    stream: tokio::net::UnixStream,
    shutdown: CancellationToken,
    connections: Arc<AtomicUsize>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader
        .read_line(&mut line)
        .await
        .context("read hook request")?;

    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(line.trim()) {
        let method = raw
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if method == "tool/shutdown" {
            let connected = connections.load(Ordering::Acquire);
            info!(
                source = Source::DaemonLifecycle.as_str(),
                connections = connected,
                "shutdown requested via stop command",
            );
            let response = serde_json::json!({ "status": "ok", "connections": connected });
            let mut payload = serde_json::to_vec(&response)?;
            payload.push(b'\n');
            writer.write_all(&payload).await?;
            writer.shutdown().await?;
            shutdown.cancel();
            return Ok(());
        }

        // `tool/version` (from `catenary version`) works without a session:
        // mirror the session-aware dispatcher so a session-less daemon (e.g.
        // `lsp = false`) still reports its version.
        if method == "tool/version" {
            let response = serde_json::json!({ "version": env!("CATENARY_VERSION") });
            let mut payload = serde_json::to_vec(&response)?;
            payload.push(b'\n');
            writer.write_all(&payload).await?;
            writer.shutdown().await?;
            return Ok(());
        }

        info!(
            source = Source::DaemonDispatch.as_str(),
            method, "hook request (passthrough)",
        );
    }

    // Empty response = "allow" for all hook types.
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;

    Ok(())
}

/// Looks up or creates a per-session [`Session`] + [`HookRouter`] pair.
///
/// Each `session_id` gets its own `Session` (via
/// [`Session::new_for_daemon`]) with independent editing state, sharing the
/// primary's parent-context queue. The `HookRouter` wraps the per-session
/// `Session` with its own turn counter and debounce state.
///
/// Populates the session's board metadata on first creation; the daemon
/// snapshot (`state.json`) surfaces it to the TUI dashboard.
#[cfg(unix)]
fn get_or_create_router(
    ctx: &HookDispatchContext,
    session_id: &str,
    raw: &serde_json::Value,
) -> Arc<HookRouter> {
    // Set inside `or_insert_with` (first creation only) so the `session_connect`
    // milestone is emitted *after* the registry lock drops below — never under
    // it, since `record_milestone` takes the snapshot lock (ticket 05a's
    // lock-order rule).
    let mut connect_summary: Option<String> = None;
    let router = ctx
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(session_id.to_string())
        .or_insert_with(|| {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id, "creating session",
            );
            let session_id_arc: Arc<str> = session_id.into();
            let session = Arc::new(Session::new_for_daemon(
                &ctx.primary,
                session_id_arc,
                Some(ctx.editing_guardrail.clone()),
            ));

            // Board metadata from this session's own hook payload (ticket 05):
            // the `format` field is the host label (client_name), the connect
            // time, and the session's own workspace roots. The daemon snapshot
            // (`state.json`) surfaces this to the TUI dashboard.
            let client_name = raw.get("format").and_then(|v| v.as_str());
            connect_summary = Some(format!(
                "session connected ({})",
                client_name.unwrap_or("unknown")
            ));
            // One ISO format across the snapshot (now_iso → millis + 'Z').
            let started_at = crate::state_snapshot::now_iso();
            let meta = SessionMeta {
                client_name: client_name.map(str::to_string),
                started_at,
                roots: extract_session_roots(raw),
            };

            let router = Arc::new(HookRouter::new(
                session.clone(),
                session.instance_id.clone(),
                session_id.to_string(),
            ));

            SessionEntry { router, meta }
        })
        .router
        .clone();

    // Registry guard dropped at the statement above. A first-creation emits the
    // `session_connect` milestone now, with the snapshot lock taken clear of the
    // registry lock (ticket 08).
    if let Some(summary) = connect_summary {
        router.session.record_milestone(
            crate::state_snapshot::MilestoneKind::SessionConnect,
            summary,
            Some(session_id.to_string()),
        );
    }

    // Bump `last_seen` on EVERY dispatch, not only on create. Every agent tool
    // call funnels through here — including the Bash hooks that wrap
    // `catenary grep`/`glob`/`diagnostics` (only those commands' own
    // subprocess IPC bypasses the session) — so `last_seen` is the one uniform
    // liveness signal a hook session has, far richer than `last_action`, which
    // moves only on edit / diagnostics. The bump takes the session's own
    // lock and marks the snapshot dirty (coalesced, ticket 04's `$/progress`
    // I/O model) — the registry guard above dropped at the end of its
    // statement, so the heavily-shared registry lock is never held across it
    // (ticket 05a).
    router.session.touch_last_seen();

    router
}

/// Appends the "outside tracked roots" advisory to a diagnostics `output`
/// when `filtered` edits were dropped for lack of LSP coverage.
///
/// This keeps the diagnostics batch honest. A mixed batch — some edited
/// files covered, some not — otherwise renders results for the covered
/// files alone, with no signal that the rest went unchecked; a silent,
/// incomplete batch is the "lying batch" the editing workflow exists to
/// avoid. The all-uncovered case (empty `output`, `filtered > 0`) reduces
/// to the note alone — so a bare `catenary diagnostics` after only out-of-root
/// edits never renders the bare `[no edited files]` lie (bug 58). Returns
/// `output` unchanged when nothing was filtered.
///
/// `filtered_roots` carries the distinct enclosing project roots of those
/// filtered edits (walk repository markers up from each). When non-empty the note names
/// them — "no language servers running for `~/Projects/Lattice`" — pointing at
/// what a `catenary pin` would mount (ephemeral-roots ticket 01); when
/// empty (no detectable root) it falls back to the plain count.
#[cfg(unix)]
fn with_out_of_roots_note(
    output: String,
    filtered: usize,
    filtered_roots: &std::collections::BTreeSet<PathBuf>,
) -> String {
    if filtered == 0 {
        return output;
    }
    let plural = if filtered == 1 { "" } else { "s" };
    let note = if filtered_roots.is_empty() {
        format!(
            "({filtered} edit{plural} outside tracked roots \u{2014} not checked; see `catenary roots -h`)",
        )
    } else {
        let named = filtered_roots
            .iter()
            .map(|root| crate::bridge::compress_home(root))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "({filtered} edit{plural} outside tracked roots \u{2014} no language servers running for {named}; see `catenary roots -h`)",
        )
    };
    if output.is_empty() {
        note
    } else {
        format!("{output}\n{note}")
    }
}

/// Decides whether an edited file should auto-mount its enclosing git worktree.
///
/// Implements the subagent auto-mount predicate (workstream 30, ticket 1a): a
/// `PreToolUse` edit landing in a worktree of a project this session already
/// tracks should mount that worktree so it gets its own rust-analyzer. Returns
/// the **worktree toplevel** to mount, or `None` when no mount is warranted.
///
/// Returns `Some(worktree)` iff all hold:
///
/// - the file resolves to an enclosing git worktree
///   ([`crate::companions::enclosing_worktree_root`]);
/// - that worktree is **not already** a tracked root (idempotent — already
///   mounted, by any contributor);
/// - the worktree's [`canonical_project_root`](crate::companions::canonical_project_root)
///   **is** a tracked root and is **distinct** from the worktree itself.
///
/// The canonical root only *authorizes* the mount (per ADR 016 — a worktree of
/// a project the session already works on); it is never itself returned for
/// mounting. The distinctness check makes the main agent editing inside an
/// already-tracked checkout a no-op: there the worktree *is* its canonical root,
/// which is already tracked, so the "not already tracked" clause rejects it.
///
/// `tracked` is the current global root set; membership is compared after
/// canonicalizing the worktree path so it lines up with the canonicalized roots
/// the tracker stores.
#[cfg(unix)]
fn worktree_to_auto_mount(file_path: &Path, tracked: &HashSet<PathBuf>) -> Option<PathBuf> {
    let worktree = crate::companions::enclosing_worktree_root(file_path)?;
    // Canonicalize so the comparison matches the tracker's canonicalized roots
    // (falls back to the raw path when the worktree no longer exists on disk).
    let worktree = worktree.canonicalize().unwrap_or(worktree);

    // Idempotent: already mounted (by this agent or any other contributor).
    if tracked.contains(&worktree) {
        return None;
    }

    let canonical = crate::companions::canonical_project_root(&worktree);
    let canonical = canonical.canonicalize().unwrap_or(canonical);

    // Authorize the mount iff the worktree's canonical project root is tracked
    // and distinct from the worktree (a linked worktree, not the main checkout).
    if canonical != worktree && tracked.contains(&canonical) {
        Some(worktree)
    } else {
        None
    }
}

/// Canonicalizes a subagent worktree path and builds its
/// `worktree:{session_id}:{path}` root-contributor key, returning both the
/// canonical path (for logging) and the key.
///
/// Uses the same `.canonicalize().unwrap_or(raw)` fallback
/// [`worktree_to_auto_mount`] applies at mount, so the `WorktreeRemove` and
/// `SubagentStop` teardown routes rebuild the exact key the mount installed
/// whenever the worktree dir still exists on disk.
#[cfg(unix)]
fn worktree_contributor(session_id: &str, path: &Path) -> (PathBuf, String) {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let contributor = format!("worktree:{session_id}:{}", canonical.display());
    (canonical, contributor)
}

/// Resolves the `(contributor, worktree)` a `SubagentStop` should reap — identity
/// first, cwd second (misc 150 hardening).
///
/// 1. **Identity.** When `agent_id` is non-empty and its identity contributor
///    `worktree:{session_id}:{agent_id}` is live, reap that. The registry supplies
///    the registered path (for the log + drift telemetry); when it disagrees with
///    the cwd-enclosing root the registry wins (we reap the identity contributor
///    either way) and the divergence is logged at debug.
/// 2. **cwd.** Otherwise resolve the *enclosing* worktree root of `cwd`
///    ([`crate::companions::enclosing_worktree_root`]) and key on THAT — never an
///    exact match on the raw cwd, so a final `cd` into a subdirectory of the
///    worktree still reaps. This covers foreign/legacy worktrees with no agent
///    identity (path-keyed mounts) and the registry-miss case.
///
/// Returns `None` when neither route yields a candidate (no agent identity mount
/// and no enclosing worktree for `cwd`).
#[cfg(unix)]
fn resolve_stop_reap_target(
    ctx: &HookDispatchContext,
    tracker: &RootTracker,
    session_id: &str,
    agent_id: &str,
    cwd: Option<&str>,
) -> Option<(String, PathBuf)> {
    // The cwd route: the enclosing worktree root, canonicalized to line up with
    // the tracker's canonical root values.
    let cwd_route = cwd.and_then(|c| {
        crate::companions::enclosing_worktree_root(Path::new(c)).map(|root| {
            let root = root.canonicalize().unwrap_or(root);
            (format!("worktree:{session_id}:{}", root.display()), root)
        })
    });

    if !agent_id.is_empty() {
        let identity = format!("worktree:{session_id}:{agent_id}");
        if tracker.has_contributor(&identity) {
            let registered = ctx
                .worktree_registry
                .path_for_identity(session_id, agent_id);
            // Drift telemetry: the registry (identity) path vs the cwd-enclosing
            // root. Prefer the registry; log the disagreement at debug.
            if let (Some(reg), Some((_, cwd_root))) = (&registered, &cwd_route)
                && reg != cwd_root
            {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    agent_id = agent_id,
                    registry = %reg.display(),
                    cwd_root = %cwd_root.display(),
                    "registry/cwd worktree divergence at subagent stop — preferring the registry",
                );
            }
            let worktree = registered
                .or_else(|| cwd_route.as_ref().map(|(_, r)| r.clone()))
                .unwrap_or_else(|| PathBuf::from(cwd.unwrap_or_default()));
            return Some((identity, worktree));
        }
    }
    cwd_route
}

/// Tears down a mounted subagent worktree root: drops the `contributor` and its
/// in-memory deletion watch, then re-syncs the reduced root set so the worktree's
/// language servers shut down.
///
/// Idempotent — [`RootTracker::remove_contributor`] of an absent key is a no-op,
/// so a caller may invoke it whether or not the key is currently mounted.
/// Internal lifecycle only: `debug!`, never `warn!`/`error!`
/// (`docs/src/tracing-conventions`). `trigger` names the firing edge
/// (`"worktree removal"`, `"subagent stop"`) for the teardown log.
#[cfg(unix)]
async fn reap_worktree_root(
    ctx: &HookDispatchContext,
    tracker: &RootTracker,
    session_id: &str,
    contributor: &str,
    worktree: &Path,
    trigger: &str,
) {
    tracker.remove_contributor(contributor);
    // Drop the in-memory deletion watch too (ticket 05) so it never outlives the
    // root; idempotent vs the watch reaper and the GC.
    if let Some(ref watcher) = ctx.worktree_watcher {
        watcher.unregister(contributor);
    }
    // Drop the worktree-class idle clock (misc 150) so it never outlives the root.
    ctx.worktree_mounts.remove(contributor);
    let global = tracker.global_roots_rich();
    if let Err(e) = ctx.primary.sync_roots(global).await {
        debug!(
            source = Source::DaemonDispatch.as_str(),
            "root sync after {trigger} teardown failed: {e}",
        );
    }
    debug!(
        source = Source::DaemonDispatch.as_str(),
        session_id = %session_id,
        worktree = %worktree.display(),
        contributor = %contributor,
        "tore down worktree root at {trigger}",
    );
}

/// Build one `catenary worktree ls` row (misc 151) from a sidecar meta and the
/// live mount/blocked map.
///
/// Merges the durable sidecar (path, class, creator, age) with the daemon's live
/// state: `dirty` uses the disposal invariant for agent worktrees (uncommitted or
/// `HEAD` moved) and the working-tree status for feats (whose local commits are
/// expected — shown via ahead/behind); `root_state` is `mounted` / `blocked` /
/// `unmounted`. Feats rows carry `ahead`/`behind` upstream counts when available.
#[cfg(unix)]
fn worktree_ls_row(
    meta: &crate::worktree_create::WorktreeMeta,
    mounts: &HashMap<PathBuf, bool>,
) -> serde_json::Value {
    let is_feat = meta.class == crate::worktree_create::WORKTREE_CLASS_FEAT;
    let present = meta.worktree.exists();
    let dirty = if present {
        if is_feat {
            crate::worktree_dispose::worktree_status_dirty(&meta.worktree)
        } else {
            !crate::worktree_dispose::is_disposable_clean(meta)
        }
    } else {
        false
    };
    let root_state = if present {
        match mounts.get(&meta.worktree) {
            Some(true) => "blocked",
            Some(false) => "mounted",
            None => "unmounted",
        }
    } else {
        "unmounted"
    };
    let creator = if is_feat {
        "cli".to_string()
    } else {
        format!(
            "{} / {}",
            meta.session_id,
            meta.agent_id.as_deref().unwrap_or(meta.name.as_str()),
        )
    };
    let mut row = serde_json::json!({
        "path": meta.worktree.display().to_string(),
        "class": meta.class,
        "creator": creator,
        "created_at": meta.created_at,
        "present": present,
        "dirty": dirty,
        "root_state": root_state,
    });
    if is_feat
        && present
        && let Some((behind, ahead)) = crate::worktree_dispose::feat_ahead_behind(&meta.worktree)
    {
        row["ahead"] = serde_json::Value::from(ahead);
        row["behind"] = serde_json::Value::from(behind);
    }
    row
}

/// Handle a `tool/worktree-rm` request (misc 151): load the sidecar, reap any
/// live mount, and dispose class-appropriately, returning the CLI response.
///
/// An agent worktree removes on the caller's captured-work assertion (the
/// force-shaped landing path — firehose-logged); a feats worktree refuses dirty
/// (uncommitted or unpushed). A path with no sidecar is never ours to touch.
#[cfg(unix)]
async fn handle_worktree_rm(
    ctx: &HookDispatchContext,
    raw: &serde_json::Value,
) -> serde_json::Value {
    let Some(raw_path) = raw.get("path").and_then(|v| v.as_str()) else {
        return serde_json::json!({ "status": "error", "message": "missing path" });
    };
    let worktree = Path::new(raw_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(raw_path));
    let sidecar = crate::worktree_create::sidecar_path(&worktree);
    let Some(meta) = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|c| serde_json::from_str::<crate::worktree_create::WorktreeMeta>(&c).ok())
    else {
        return serde_json::json!({
            "status": "not_ours",
            "message": format!("{} is not a Catenary-managed worktree", worktree.display()),
        });
    };

    // Reap any live mount so the worktree's servers shut down before removal.
    if let Some(tracker) = &ctx.root_tracker
        && let Some((contributor, _)) = tracker
            .contributors_with_prefix("worktree:")
            .into_iter()
            .find(|(_, roots)| roots.iter().any(|r| r == &worktree))
    {
        let sid = worktree_contributor_session_id(&contributor)
            .unwrap_or("default")
            .to_string();
        reap_worktree_root(ctx, tracker, &sid, &contributor, &worktree, "worktree rm").await;
    }

    let disposition = if meta.class == crate::worktree_create::WORKTREE_CLASS_FEAT {
        crate::worktree_dispose::remove_feat(&meta)
    } else {
        // The captured-work assertion is the deliberate force-shaped path — record
        // it on the firehose (the deliberate user-relevant lifecycle exception is
        // the dirty-kept nag, not this routine landing).
        info!(
            source = Source::DaemonDispatch.as_str(),
            worktree = %worktree.display(),
            "worktree rm: agent worktree removed on the caller's captured-work assertion",
        );
        crate::worktree_dispose::remove_agent_asserted(&meta)
    };
    worktree_rm_response(&disposition, &worktree)
}

/// Map a disposal [`Disposition`](crate::worktree_dispose::Disposition) to the
/// `catenary worktree rm` CLI response.
#[cfg(unix)]
fn worktree_rm_response(
    disposition: &crate::worktree_dispose::Disposition,
    worktree: &Path,
) -> serde_json::Value {
    use crate::worktree_dispose::Disposition;
    match disposition {
        Disposition::Disposed | Disposition::Remnant => serde_json::json!({
            "status": "ok",
            "removed": true,
            "path": worktree.display().to_string(),
        }),
        Disposition::KeptDirty { reason } => serde_json::json!({
            "status": "kept",
            "removed": false,
            "message": reason,
        }),
        Disposition::Refused { reason } => serde_json::json!({
            "status": "refused",
            "removed": false,
            "message": reason,
        }),
        Disposition::NotOurs => serde_json::json!({
            "status": "not_ours",
            "removed": false,
            "message": "not a Catenary-managed worktree",
        }),
    }
}

/// Load a worktree's [`WorktreeMeta`](crate::worktree_create::WorktreeMeta) from
/// its sidecar (the durable disposal record), for a background dispose after a
/// registry miss (daemon restarted since creation).
#[cfg(unix)]
fn load_meta_from_sidecar(worktree: &Path) -> Option<crate::worktree_create::WorktreeMeta> {
    let sidecar = crate::worktree_create::sidecar_path(worktree);
    let contents = std::fs::read_to_string(sidecar).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Dispose a worktree in a background task (misc 151 triggers 1/2/4).
///
/// Blocking (git subprocesses) — call from `spawn_blocking`. On a clean dispose
/// the registry entry is dropped; on a dirty keep the parent is surfaced
/// ([`surface_dirty_kept`]) unless the removal was host-initiated (`WorktreeRemove`
/// logs the divergence inside [`crate::worktree_dispose::dispose`] instead). A
/// path with no registry entry and no sidecar is silently skipped (never ours).
#[cfg(unix)]
fn dispose_worktree_in_background(
    registry: &WorktreeRegistry,
    parent_context: &crate::bridge::ParentContextQueue,
    session_id: &str,
    agent_id: &str,
    worktree: &Path,
    host_initiated: bool,
) {
    let Some(meta) = registry
        .get(worktree)
        .or_else(|| load_meta_from_sidecar(worktree))
    else {
        return;
    };
    let disposition = crate::worktree_dispose::dispose(&meta, host_initiated);
    match &disposition {
        crate::worktree_dispose::Disposition::Disposed
        | crate::worktree_dispose::Disposition::Remnant => registry.forget(worktree),
        crate::worktree_dispose::Disposition::KeptDirty { .. } if !host_initiated => {
            surface_dirty_kept(parent_context, session_id, agent_id, worktree);
        }
        _ => {}
    }
}

/// Surface a dirty worktree kept at `SubagentStop` to the *parent agent* (misc
/// 151, D-1).
///
/// The retired notification queue's user `systemMessage` leg is gone (tui-rework
/// 04): the notice's actionable audience is the parent agent, delivered as
/// `additionalContext` ([`queue_parent_additional_context`]). The `warn!` stays
/// as the firehose/log record of the event (queryable via `catenary query`), no
/// longer a user notification.
#[cfg(unix)]
fn surface_dirty_kept(
    parent_context: &crate::bridge::ParentContextQueue,
    session_id: &str,
    agent_id: &str,
    worktree: &Path,
) {
    let message = format!(
        "subagent `{agent_id}` left a dirty worktree at `{}` (kept; land its work \
         or `catenary worktree rm` it)",
        worktree.display(),
    );
    warn!(
        source = Source::DaemonDispatch.as_str(),
        session_id = %session_id,
        "{message}",
    );
    queue_parent_additional_context(parent_context, session_id, message);
}

/// Deliver the dirty-kept notice to the *parent agent* as `additionalContext`
/// on its next eligible hook response (misc 151, D-1 — the cashed-in stub).
///
/// The `SubagentStop` response goes to the *stopping subagent*, not the parent,
/// so the notice is queued against the parent's `session_id` in the shared
/// [`ParentContextQueue`](crate::bridge::ParentContextQueue). The parent's own
/// hook dispatch drains it on its next allowed `PreToolUse` / `Stop` (see
/// [`HookRouter::drain_parent_context`](crate::bridge::HookRouter)), where the
/// CLI emits it as `hookSpecificOutput.additionalContext`. Session-scoped,
/// dropped on session end.
#[cfg(unix)]
fn queue_parent_additional_context(
    parent_context: &crate::bridge::ParentContextQueue,
    session_id: &str,
    message: String,
) {
    parent_context.queue(session_id, message);
}

/// The `id`s of background subagents reported **running** in a stop payload's
/// `background_tasks` array (misc 151 D-2).
///
/// The maintainer's awaiting-a-background-subagent case: a worktree whose owning
/// agent is still running must never be nagged. Reads `host_payload.background_tasks`
/// (the forwarded live field), falling back to a top-level array. The `id`↔agent
/// correspondence follows the ticket's live-verified field; a convention drift is
/// caught by the mount check (a running agent's root is mounted) as a backstop.
#[cfg(unix)]
fn running_background_agent_ids(raw: &serde_json::Value) -> Vec<String> {
    let tasks = raw
        .get("host_payload")
        .and_then(|hp| hp.get("background_tasks"))
        .or_else(|| raw.get("background_tasks"))
        .and_then(|v| v.as_array());
    let Some(tasks) = tasks else {
        return Vec::new();
    };
    tasks
        .iter()
        .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("running"))
        .filter_map(|t| t.get("id").and_then(|i| i.as_str()).map(String::from))
        .collect()
}

/// The lingering-worktree nag message, or `None` when nothing qualifies (misc 151
/// D-2).
///
/// Collects the session's registered worktrees satisfying ALL of: dir present,
/// root **not** mounted, owning agent **not** running in `background_tasks`, and
/// not yet nagged this daemon lifetime (marked here — once per worktree). Empty
/// → `None` (no block). The caller blocks the stop once with this list; the
/// `stop_hook_active` retry passes.
#[cfg(unix)]
fn lingering_worktree_nag(
    ctx: &HookDispatchContext,
    session_id: &str,
    raw: &serde_json::Value,
) -> Option<String> {
    let running = running_background_agent_ids(raw);
    let mounted: HashSet<PathBuf> = ctx
        .worktree_mounts
        .mounted_roots()
        .into_iter()
        .map(|(path, _)| path)
        .collect();

    let mut lingering = Vec::new();
    for meta in ctx.worktree_registry.metas_for_session(session_id) {
        if !meta.worktree.exists() {
            continue; // dir gone — a remnant, not a lingering worktree
        }
        if mounted.contains(&meta.worktree) {
            continue; // root still mounted (a live or blocked agent)
        }
        let owner = meta.agent_id.as_deref().unwrap_or(meta.name.as_str());
        if running
            .iter()
            .any(|id| id == owner || id == &meta.name || id == &format!("agent-{owner}"))
        {
            continue; // owning agent is still running in the background
        }
        if !ctx.worktree_registry.mark_nagged(&meta.worktree) {
            continue; // already nagged this daemon lifetime (once per worktree)
        }
        lingering.push(meta.worktree.clone());
    }

    if lingering.is_empty() {
        return None;
    }
    let list = lingering
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let count = lingering.len();
    let noun = if count == 1 { "worktree" } else { "worktrees" };
    Some(format!(
        "{count} agent {noun} linger (root unmounted, owner not running):\n{list}\n\
         Land their work or `catenary worktree rm <path>` each. (Nagged once per worktree.)"
    ))
}

/// Streams a grep outcome as a chunk-frame sequence (misc 140 phase 2).
///
/// One [`GrepFrame::Chunk`] per rendered hunk in global sort order, then one
/// [`GrepFrame::End`] terminator carrying the count/skip tallies. Chunk payloads
/// are the raw hunk bytes (trailing newlines intact); the CLI concatenates them
/// and applies the trailing-whitespace trim, reproducing the pre-framing output
/// byte-for-byte. Spooled hunks are read one at a time on a blocking task so a
/// giant hunk never pins the runtime — peak memory is the path index plus one
/// hunk in flight.
///
/// # Errors
///
/// Returns an error if a spool read fails or the socket write fails.
#[cfg(unix)]
async fn write_grep_frames<W>(writer: &mut W, outcome: Result<GrepOutcome>) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match outcome {
        Ok(GrepOutcome::Count {
            matches,
            files,
            skipped,
        }) => {
            write_grep_frame(
                writer,
                &GrepFrame::End {
                    matches: Some(matches),
                    files: Some(files),
                    skipped,
                },
            )
            .await?;
        }
        Ok(GrepOutcome::Rendered { output, skipped }) => {
            let (chunks, spool) = output.into_parts();
            for chunk in chunks {
                let data = match chunk {
                    HunkChunk::InMemory(s) => s,
                    HunkChunk::Spooled { offset, len } => {
                        let spool = spool
                            .clone()
                            .ok_or_else(|| anyhow!("spooled grep hunk without a spool"))?;
                        let bytes =
                            tokio::task::spawn_blocking(move || spool.read_hunk(offset, len))
                                .await
                                .map_err(|e| anyhow!("grep spool read task failed: {e}"))??;
                        String::from_utf8(bytes)
                            .map_err(|e| anyhow!("grep spool hunk is not utf-8: {e}"))?
                    }
                };
                write_grep_frame(writer, &GrepFrame::Chunk { data }).await?;
            }
            write_grep_frame(
                writer,
                &GrepFrame::End {
                    matches: None,
                    files: None,
                    skipped,
                },
            )
            .await?;
        }
        Err(e) => {
            // A grep error becomes the rendered output, exactly as the legacy
            // envelope carried it — one chunk, then a bare terminator.
            write_grep_frame(
                writer,
                &GrepFrame::Chunk {
                    data: format!("grep error: {e}"),
                },
            )
            .await?;
            write_grep_frame(
                writer,
                &GrepFrame::End {
                    matches: None,
                    files: None,
                    skipped: GrepSkips::default(),
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// Writes one [`GrepFrame`] as a single JSON line.
///
/// # Errors
///
/// Returns an error if serialization or the socket write fails.
#[cfg(unix)]
async fn write_grep_frame<W>(writer: &mut W, frame: &GrepFrame) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

/// Materializes a [`ShapedOutput`] into the single trimmed output string the
/// legacy single-envelope [`GrepResponse`] carries — the version-skew compat
/// path for a CLI that predates chunked framing.
///
/// Reads every hunk (including spooled ones) and applies the trailing-whitespace
/// trim the pre-framing render applied, so the bytes match a pre-framing daemon
/// exactly. Unbounded in memory, but reached only by a legacy CLI.
///
/// # Errors
///
/// Returns an error if a spool read fails or a hunk is not valid UTF-8.
#[cfg(unix)]
fn shaped_to_string(output: ShapedOutput) -> Result<String> {
    let (chunks, spool) = output.into_parts();
    let mut out = String::new();
    for chunk in chunks {
        match chunk {
            HunkChunk::InMemory(s) => out.push_str(&s),
            HunkChunk::Spooled { offset, len } => {
                let spool = spool
                    .as_ref()
                    .ok_or_else(|| anyhow!("spooled grep hunk without a spool"))?;
                let bytes = spool.read_hunk(offset, len)?;
                out.push_str(
                    &String::from_utf8(bytes)
                        .map_err(|e| anyhow!("grep spool hunk is not utf-8: {e}"))?,
                );
            }
        }
    }
    let trimmed_len = out.trim_end().len();
    out.truncate(trimmed_len);
    Ok(out)
}

/// Handles a single hook connection with session-aware dispatch.
///
/// Reads the JSON request, extracts `session_id` for routing, looks up
/// (or creates) the per-session [`HookRouter`], dispatches the request,
/// logs the protocol pair, and writes the response.
#[cfg(unix)]
#[allow(clippy::too_many_lines, reason = "sequential protocol steps")]
async fn handle_hook_dispatch(
    stream: tokio::net::UnixStream,
    ctx: HookDispatchContext,
    shutdown: CancellationToken,
    connections: Arc<AtomicUsize>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader
        .read_line(&mut line)
        .await
        .context("read hook request")?;

    let raw: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| anyhow!("Invalid hook request: {e}"))?;
    let method = raw
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Handle shutdown from `catenary stop`. The ack reports the live MCP
    // connection count so the CLI can warn that those sessions will lose
    // tooling until each reconnects via `/mcp`.
    if method == "tool/shutdown" {
        let connected = connections.load(Ordering::Acquire);
        info!(
            source = Source::DaemonLifecycle.as_str(),
            connections = connected,
            "shutdown requested via stop command",
        );
        let response = serde_json::json!({ "status": "ok", "connections": connected });
        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        shutdown.cancel();
        return Ok(());
    }

    // ── Report daemon version ──────────────────────────────────
    //
    // `tool/version` is sent by `catenary version`. Returns this
    // daemon binary's embedded `CATENARY_VERSION` so the CLI can
    // compare it against its own and flag a stale daemon.
    if method == "tool/version" {
        let response = serde_json::json!({ "version": env!("CATENARY_VERSION") });
        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── List tracked roots ─────────────────────────────────────
    //
    // `tool/roots-ls` is sent by bare `catenary roots`. Returns all
    // tracked workspace roots with their contributor sources.
    if method == "tool/roots-ls" {
        let roots = ctx
            .root_tracker
            .as_ref()
            .map_or_else(Vec::new, RootTracker::list_roots);

        let roots_json: Vec<serde_json::Value> = roots
            .into_iter()
            .map(|(path, sources)| {
                // Classify by contributor prefix (ticket 02): an ephemeral root
                // is held only by `ephemeral:*` contributors. The CLI renders the
                // class distinctly (`catenary roots ls`).
                let ephemeral = root_is_ephemeral(&sources);
                serde_json::json!({
                    "path": path.display().to_string(),
                    "sources": sources,
                    "ephemeral": ephemeral,
                })
            })
            .collect();

        let response = serde_json::json!({
            "status": "ok",
            "roots": roots_json,
        });

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── worktree ls — the registry+sidecar view (misc 151) ─────────────
    //
    // `tool/worktree-ls` is sent by `catenary worktree ls`. Scans the agent and
    // feats sidecars, merges the daemon's live mount/blocked state, and returns a
    // row per worktree (path, class, creator, age, dirty, root state, and — for
    // feats — ahead/behind upstream). Daemon-level, no per-session state.
    if method == "tool/worktree-ls" {
        let mounts: HashMap<PathBuf, bool> =
            ctx.worktree_mounts.mounted_roots().into_iter().collect();
        let mut metas =
            crate::worktree_create::scan_sidecars(&crate::paths::agents_worktrees_dir());
        metas.extend(crate::worktree_create::scan_sidecars_recursive(
            &crate::paths::feats_worktrees_dir(),
        ));

        let rows: Vec<serde_json::Value> = metas
            .into_iter()
            .map(|meta| worktree_ls_row(&meta, &mounts))
            .collect();

        let response = serde_json::json!({ "status": "ok", "worktrees": rows });
        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── worktree rm — one removal verb, class-appropriate (misc 151) ───
    //
    // `tool/worktree-rm` is sent by `catenary worktree rm <path>`. Loads the
    // sidecar, reaps any live mount (so its servers shut down), then disposes by
    // class: an agent worktree removes on the caller's captured-work assertion
    // (the force-shaped landing path, firehose-logged); a feats worktree refuses
    // dirty (uncommitted or unpushed). Daemon-level.
    if method == "tool/worktree-rm" {
        let response = handle_worktree_rm(&ctx, &raw).await;
        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // Extract session_id for routing. Falls back to "default" for hooks
    // that don't carry a session_id (backward compatibility).
    let session_id = raw
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    // ── Antigravity teaching first-sighting (teaching-surface ticket 03) ──
    //
    // Fires on every Antigravity `PreInvocation` (before each model call). The
    // ledger is keyed on `conversationId` (the Antigravity `session_id`) and
    // answers whether this is the conversation's first sighting — so the CLI
    // injects the ticket-01 teaching payload as a persisted `injectSteps`
    // `userMessage` exactly once per conversation, robust to `invocationNum`
    // resume semantics. Daemon-authoritative and check-and-record-atomic:
    // `see` returns `true` only for the first call per conversation.
    //
    // Short-circuits before get_or_create_router: this is a daemon-global
    // ledger concern with no per-session editing/notification state.
    if method == "pre-invocation/first-sighting" {
        let scope_id = uuid::Uuid::new_v4().to_string();
        let inject = ctx.first_sightings.see(&session_id);
        let response = serde_json::json!({ "inject": inject }).to_string();

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &response,
            "outgoing hook response",
        );

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Session-end cleanup ───────────────────────────────────────
    //
    // Fires when the host CLI sends a SessionEnd hook (exit, /clear,
    // resume, logout). Cleans up session-scoped state: editing
    // guardrail, parent-context queue, session registry, and roots.
    //
    // Short-circuits before get_or_create_router to avoid creating
    // a new session just to immediately clean it up.
    if method == "session-end/cleanup" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        // Release editing guardrail locks (idempotent if MCP
        // disconnect already ran).
        ctx.editing_guardrail.release_all(&session_id);

        // Drop any undelivered parent-agent context for this session
        // (misc 151 — session-scoped, dropped on session end).
        ctx.primary.parent_context.remove_session(&session_id);

        // Remove the session from the registry.
        ctx.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session_id);

        // Best-effort removal from the board — mark the snapshot dirty so the
        // next flush drops it. Not a tombstone: a Claude resume re-creates the
        // entry via `get_or_create_router`, and Antigravity sends no
        // `session-end` at all. `last_seen` is the authoritative liveness signal
        // (ticket 05a). The disconnect milestone records the (best-effort) end on
        // the activity ring (ticket 08).
        ctx.primary.record_milestone(
            crate::state_snapshot::MilestoneKind::SessionDisconnect,
            "session disconnected",
            Some(session_id.clone()),
        );
        ctx.primary.touch_snapshot();

        if let Some(ref tracker) = ctx.root_tracker {
            // Leak backstop (workstream 30, ticket 03): reclaim any of THIS
            // session's worktree roots whose `WorktreeRemove` was missed at a
            // graceful end. The `session_id` baked into the contributor key lets
            // the sweep find them without enumerating paths. Routine cleanup, so
            // debug/info — never warn (which reaches the user notification queue).
            let prefix = format!("worktree:{session_id}:");
            let removed = tracker.remove_contributors_with_prefix(&prefix);
            // Drop this session's in-memory deletion watches too (ticket 05) so
            // they never outlive the roots; idempotent vs the reaper and the GC.
            if let Some(ref watcher) = ctx.worktree_watcher {
                watcher.unregister_with_prefix(&prefix);
            }
            // Drop this session's worktree idle clocks too (misc 150).
            ctx.worktree_mounts.remove_prefix(&prefix);
            // Drop this session's subagent board entries too (tui-rework 03).
            ctx.subagents.clear_session(&session_id);
            if removed > 0 {
                info!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    count = removed,
                    "session ended: swept leaked worktree roots",
                );
            } else {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    "session ended: no worktree roots to sweep",
                );
            }

            // Sync the reduced root set.
            let global = tracker.global_roots_rich();
            if let Err(e) = ctx.primary.sync_roots(global).await {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    "root sync after session end failed: {e}",
                );
            }

            info!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                "session ended: roots cleaned up",
            );
        }

        // misc 151 trigger 2: dispose this session's CLEAN agent worktrees — the
        // resume window is over. Dirty ones are kept (they become cross-session
        // orphans surfaced at the next SessionStart, not the now-gone session's
        // queue). The roots were just swept above, so the worktrees are
        // unmounted; the git work runs in the background so session-end is prompt.
        let session_metas = ctx.worktree_registry.metas_for_session(&session_id);
        if !session_metas.is_empty() {
            let registry = ctx.worktree_registry.clone();
            tokio::task::spawn_blocking(move || {
                for meta in session_metas {
                    let disposition = crate::worktree_dispose::dispose(&meta, false);
                    if disposition.is_disposed() {
                        registry.forget(&meta.worktree);
                    }
                }
            });
        }

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "",
            "outgoing hook response",
        );

        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── WorktreeCreate registration + payload log (misc 144 / misc 150) ──
    //
    // The `WorktreeCreate` hook creates the worktree locally (`git worktree add`
    // under the agents subtree, print the path) and forwards two things here: its
    // full host payload — for the queryable firehose (`catenary query --kind
    // hook`, the schema-verification surface) — and the `worktree_meta`
    // registration, which records the identity→(path, metadata) map in the daemon
    // registry (the in-memory half; the sidecar is the durable half). The response
    // is empty.
    if method == "worktree-create/log-payload" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        // Register the created worktree (misc 150). Populated here on every live
        // create; also rehydrated from sidecars at startup, so a registration is
        // never lost across a restart.
        if let Some(meta_json) = raw.get("worktree_meta")
            && let Ok(meta) =
                serde_json::from_value::<crate::worktree_create::WorktreeMeta>(meta_json.clone())
        {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                worktree = %meta.worktree.display(),
                agent_id = meta.agent_id.as_deref().unwrap_or(""),
                "registered agent worktree",
            );
            ctx.worktree_registry.register(meta);
        }

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "",
            "outgoing hook response",
        );

        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Subagent worktree mount (workstream 30, ticket 03) ─────────
    //
    // Fires when the host CLI sends a SubagentStart hook — once at subagent
    // spawn (a `SendMessage` resume does NOT re-fire). Mounts the subagent's
    // `cwd` (the worktree of an `isolation:"worktree"` subagent) under
    // `worktree:{session_id}:{path}` iff its canonical project root is already
    // tracked and distinct (the same `worktree_to_auto_mount` predicate the
    // edit-mount uses — this is what closes the read-only-subagent coverage gap
    // and self-scopes to exactly the worktree subagents: a non-isolated
    // Explore/Plan subagent spawns with `cwd` = an already-tracked root, so the
    // predicate returns `None`).
    //
    // Short-circuits before get_or_create_router: this is a daemon-level root
    // concern (RootTracker), with no per-session editing/notification state.
    if method == "subagent-start/mount-worktree" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        // Record the subagent under its parent session for the board, whether or
        // not a worktree is mounted below (a subagent with no worktree still runs
        // under the session). Blank agent id (`--worktree`/foreign) records nothing.
        if let Some(agent_id) = raw.get("agent_id").and_then(|v| v.as_str()) {
            ctx.subagents
                .start(&session_id, agent_id, crate::state_snapshot::now_iso());
        }

        if let Some(ref tracker) = ctx.root_tracker
            && let Some(cwd) = raw.get("cwd").and_then(|v| v.as_str())
        {
            let roots: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();
            if let Some(worktree) = worktree_to_auto_mount(Path::new(cwd), &roots) {
                // Identity-keyed when the payload carries `agent_id`
                // (`worktree:{session_id}:{agent_id}`) so the SubagentStop reap
                // rebuilds the key from identity alone (misc 150); otherwise the
                // path-keyed form (`--worktree`/foreign, no agent identity), which
                // teardown rebuilds by canonicalizing `worktree_path`. The tracked
                // root VALUE is the canonical worktree path either way.
                let agent_id = raw.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
                let contributor = if agent_id.is_empty() {
                    format!("worktree:{session_id}:{}", worktree.display())
                } else {
                    format!("worktree:{session_id}:{agent_id}")
                };
                tracker.set_roots(&contributor, vec![worktree.clone()]);
                // Start the worktree-class idle clock (misc 150).
                ctx.worktree_mounts
                    .track(&contributor, &worktree, Instant::now());

                // Register the bounded deletion watch (ticket 05): the prompt
                // teardown trigger, since `git worktree remove` fires no
                // `WorktreeRemove`. Then a race guard — the dir may have already
                // been removed between mount and watch registration; if so, reap
                // immediately (idempotent vs the watch/GC/SessionEnd).
                if let Some(ref watcher) = ctx.worktree_watcher {
                    watcher.register(&contributor, &worktree);
                    if !worktree.exists() {
                        watcher.unregister(&contributor);
                        tracker.remove_contributor(&contributor);
                        ctx.worktree_mounts.remove(&contributor);
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            session_id = %session_id,
                            worktree = %worktree.display(),
                            contributor = %contributor,
                            "worktree dir already gone at mount — reaped immediately",
                        );
                    }
                }

                let global = tracker.global_roots_rich();
                if let Err(e) = ctx.primary.sync_roots(global).await {
                    debug!(
                        source = Source::DaemonDispatch.as_str(),
                        "root sync after subagent-start worktree mount failed: {e}",
                    );
                }
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    worktree = %worktree.display(),
                    contributor = %contributor,
                    "mounted worktree root at subagent start",
                );
            } else {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    cwd = cwd,
                    "subagent-start worktree mount skipped (cwd not a worktree of a tracked project, or already tracked)",
                );
            }
        }

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "",
            "outgoing hook response",
        );

        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Subagent worktree teardown (workstream 30, ticket 03) ──────
    //
    // Fires when the host CLI sends a WorktreeRemove hook — once, at the true
    // removal of an `isolation:"worktree"` subagent's worktree (NOT on every
    // stop, so it can't be premature). Removes the `worktree:{session_id}:{path}`
    // contributor so the worktree's rust-analyzer shuts down.
    //
    // The key must agree with the mount key by construction: canonicalize
    // `worktree_path` with the SAME `.canonicalize().unwrap_or(raw)` fallback
    // `worktree_to_auto_mount` uses at mount, so when the dir still exists both
    // ends resolve to the same canonical path. EDGE: if the dir is already gone
    // at teardown, `canonicalize` falls back to the raw path and the key may not
    // match the mount key — the slice-D daemon root-GC (reap `worktree:*` whose
    // dir is gone) is the crash-safe backstop for that.
    //
    // Short-circuits before get_or_create_router: daemon-level root concern only.
    if method == "worktree-remove/unmount-worktree" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        if let Some(ref tracker) = ctx.root_tracker
            && let Some(raw_path) = raw.get("worktree_path").and_then(|v| v.as_str())
        {
            let (canonical, path_key) = worktree_contributor(&session_id, Path::new(raw_path));
            // The mount may be identity-keyed (`worktree:{sid}:{agent_id}`), which
            // is not path-derivable, so reverse-lookup the `worktree:*` contributor
            // whose tracked root VALUE is this path (misc 150). Fall back to the
            // path-keyed form for a still-path-keyed mount that never registered.
            let contributor = tracker
                .contributors_with_prefix("worktree:")
                .into_iter()
                .find(|(_, roots)| roots.iter().any(|r| r == &canonical))
                .map_or(path_key, |(key, _)| key);
            reap_worktree_root(
                &ctx,
                tracker,
                &session_id,
                &contributor,
                &canonical,
                "worktree removal",
            )
            .await;

            // misc 151 trigger 4: the WorktreeRemove handler, armed. The host
            // decided removal, so dispose with `host_initiated` — the clean check
            // is advisory (still refuses dirty; a dirty keep logs the divergence
            // inside `dispose`, host asked/we declined). This is the LIVE leg for
            // non-git (svn/hg) worktrees (misc 148): the host fires WorktreeRemove
            // for them and expects the copy deleted, and `dispose` dispatches on
            // the sidecar VCS to a plain directory delete after its clean proof.
            // The guard is unchanged — never a path outside our scheme or without
            // a sidecar. (Dormant for git, whose worktrees the host removes itself.)
            let registry = ctx.worktree_registry.clone();
            let parent_context = ctx.primary.parent_context.clone();
            let sid = session_id.clone();
            let wt = canonical.clone();
            tokio::task::spawn_blocking(move || {
                dispose_worktree_in_background(&registry, &parent_context, &sid, "", &wt, true);
            });
        }

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "",
            "outgoing hook response",
        );

        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Blocked-on-permission (misc 150) ──────────────────────────
    //
    // Fires when the host CLI sends a `PermissionRequest` hook (a pure observer;
    // returns no decision). Marks the agent's worktree root **blocked** so the
    // idle reaper exempts it: a subagent parked at a permission prompt for hours
    // is blocked, not orphaned. Identity-scoped when the payload carries
    // `agent_id`; else the coarse fallback suspends idle expiry for ALL of this
    // session's worktree roots (prompts are rare). The flag clears on the next
    // identity event (any hook dispatch with that agent_id) or qualifying activity
    // under the root. No ceiling on the blocked state.
    //
    // Short-circuits before get_or_create_router: daemon-level root concern only.
    if method == "permission-request/blocked" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        let agent_id = raw.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
        if agent_id.is_empty() {
            let marked = ctx.worktree_mounts.mark_blocked_session(&session_id);
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                count = marked,
                "permission prompt (no agent id): suspended idle expiry for the session's worktree roots",
            );
        } else {
            let contributor = format!("worktree:{session_id}:{agent_id}");
            let marked = ctx.worktree_mounts.mark_blocked(&contributor);
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                agent_id = agent_id,
                marked = marked,
                "permission prompt: suspended idle expiry for the agent's worktree root",
            );
        }

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "",
            "outgoing hook response",
        );

        writer.write_all(b"\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Start editing confirmation ────────────────────────────────
    //
    // `tool/editing-start` is sent by `catenary editing start`
    // after the PreToolUse hook has already entered editing mode.
    // The CLI command just needs a confirmation response.
    if method == "tool/editing-start" {
        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Grep query ──────────────────────────────────────────────
    //
    // `tool/grep` is sent by `catenary grep`. Resolves relative
    // patterns against `cwd`, dispatches to the grep pipeline, and
    // returns the rendered output as a `GrepResponse`.
    if method == METHOD_GREP {
        let grep_req: GrepRequest = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("invalid grep request: {e}"))?;

        let params = grep_req.to_params();
        let parent_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();

        // Per-invocation search scope: the firehose shards this grep into its
        // own grep/<ts>_<uuid>.jsonl. The span carries the scope fields onto the
        // command record and the LSP requests it instruments. (Responses, emitted
        // on the shared LSP reader loop, fall back to the server file — same as
        // session-scoped LSP responses.)
        let search_ts = search_timestamp();
        let cwd = search_cwd(grep_req.cwd.as_deref());
        let span = tracing::info_span!(
            "search",
            search_id = %parent_id,
            tool = "grep",
            search_ts = %search_ts,
            cwd = %cwd,
        );

        span.in_scope(|| {
            emit_hook_event(
                tracing::Level::INFO,
                "cli",
                &method,
                Some(&parent_id),
                &raw.to_string(),
                "incoming hook",
            );
        });

        // Ephemeral mount (ticket 02): a searched path outside every mounted
        // root mounts its enclosing project root so the hits are LSP-enriched
        // from the fresh server. Refreshes the idle clock of any ephemeral root
        // the paths fall under. Instrumented with the search span so the mount
        // event shards into this grep's firehose scope.
        let grep_touched = resolve_touched_paths(&grep_req.paths, grep_req.cwd.as_deref());
        ensure_ephemeral_mounts(&ctx, &grep_touched, Instant::now(), "")
            .instrument(span.clone())
            .await;

        // Bound concurrent walks so one session's monster search cannot starve
        // the others (misc 140 phase 2). Held for the walk, released before the
        // results stream.
        let search_permit = ctx.search_limiter.acquire().await;

        // Race grep execution against client disconnect so a killed
        // CLI process doesn't leave the pipeline running indefinitely.
        let cancel_on_disconnect = cancel.clone();
        let outcome = tokio::select! {
            result = ctx.primary.grep.execute(&params, Some(&parent_id), &cancel).instrument(span.clone()) => result,
            () = async {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 1];
                let _ = buf_reader.read(&mut buf).await;
                cancel_on_disconnect.cancel();
            } => {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    "grep client disconnected — query cancelled",
                );
                span.in_scope(|| {
                    emit_hook_event(
                        tracing::Level::INFO,
                        "cli",
                        &method,
                        Some(&parent_id),
                        "client disconnected",
                        "outgoing hook response",
                    );
                });
                // The dropped `ShapedOutput` (if any) unlinks its spool.
                return Ok(());
            }
        };

        // The walk is done — free the permit so a queued search can proceed
        // while these results stream.
        drop(search_permit);

        if grep_req.chunked {
            // Chunked framing (misc 140 phase 2): stream one chunk frame per
            // hunk in global sort order, then a terminator carrying the tallies.
            // Peak memory is the path index plus one hunk in flight; the CLI
            // concatenates the chunk payloads and trims, reproducing the
            // pre-framing output byte-for-byte.
            write_grep_frames(&mut writer, outcome).await?;
            span.in_scope(|| {
                emit_hook_event(
                    tracing::Level::INFO,
                    "cli",
                    &method,
                    Some(&parent_id),
                    "streamed grep frames",
                    "outgoing hook response",
                );
            });
            writer.shutdown().await?;
            return Ok(());
        }

        // Legacy single-envelope response (a CLI that predates framing): build
        // the whole output into one `GrepResponse`. Unbounded in memory, but
        // reached only by a pre-framing CLI — a transient version-skew path.
        let response = match outcome {
            Ok(GrepOutcome::Rendered { output, skipped }) => GrepResponse {
                output: shaped_to_string(output)?,
                matches: None,
                files: None,
                skipped,
            },
            Ok(GrepOutcome::Count {
                matches,
                files,
                skipped,
            }) => GrepResponse {
                output: String::new(),
                matches: Some(matches),
                files: Some(files),
                skipped,
            },
            Err(e) => GrepResponse {
                output: format!("grep error: {e}"),
                matches: None,
                files: None,
                skipped: GrepSkips::default(),
            },
        };

        let mut payload = serde_json::to_vec(&response)?;

        span.in_scope(|| {
            emit_hook_event(
                tracing::Level::INFO,
                "cli",
                &method,
                Some(&parent_id),
                std::str::from_utf8(&payload).unwrap_or_default(),
                "outgoing hook response",
            );
        });

        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Glob query ──────────────────────────────────────────────
    //
    // `tool/glob` is sent by `catenary glob`. Resolves relative
    // patterns against `cwd`, dispatches to the glob pipeline, and
    // returns the rendered output as a `GlobResponse`.
    if method == METHOD_GLOB {
        let glob_req: GlobRequest = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("invalid glob request: {e}"))?;

        let params = glob_req.to_params();
        let parent_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();

        // Per-invocation search scope: the firehose shards this glob into its
        // own glob/<ts>_<uuid>.jsonl. The span carries the scope fields onto the
        // command record and the LSP requests it instruments. (Responses, emitted
        // on the shared LSP reader loop, fall back to the server file — same as
        // session-scoped LSP responses.)
        let search_ts = search_timestamp();
        let cwd = search_cwd(glob_req.cwd.as_deref());
        let span = tracing::info_span!(
            "search",
            search_id = %parent_id,
            tool = "glob",
            search_ts = %search_ts,
            cwd = %cwd,
        );

        span.in_scope(|| {
            emit_hook_event(
                tracing::Level::INFO,
                "cli",
                &method,
                Some(&parent_id),
                &raw.to_string(),
                "incoming hook",
            );
        });

        // Ephemeral mount (ticket 02): an outlined path outside every mounted
        // root mounts its enclosing project root so the listing is enriched from
        // the fresh server. Refreshes the idle clock of any covering ephemeral
        // root. Instrumented with the search span so the mount event shards into
        // this glob's firehose scope.
        let glob_touched = resolve_touched_paths(&glob_req.paths, glob_req.cwd.as_deref());
        ensure_ephemeral_mounts(&ctx, &glob_touched, Instant::now(), "")
            .instrument(span.clone())
            .await;

        // Bound concurrent walks so one session's monster listing cannot starve
        // the others (misc 140 phase 2) — the same shared limiter as grep. Held
        // for the walk; dropped at the end of the block.
        let _search_permit = ctx.search_limiter.acquire().await;

        // Race glob execution against client disconnect so a killed
        // CLI process doesn't leave the pipeline running indefinitely.
        let cancel_on_disconnect = cancel.clone();
        let response = tokio::select! {
            result = ctx.primary.glob.execute(&params, Some(&parent_id), &cancel).instrument(span.clone()) => {
                match result {
                    Ok(GlobOutcome::Rendered { output, no_match_indices }) => {
                        // Map each zero-match index back to the argument's
                        // ORIGINAL spelling. `to_params` resolves `glob_req.paths`
                        // to the absolute `params.paths` 1:1 in order, so the
                        // index the glob pipeline reports lines up with
                        // `glob_req.paths` — showing what the agent typed, the
                        // way `path does not exist` does (misc 118).
                        let no_match_patterns = no_match_indices
                            .into_iter()
                            .filter_map(|i| glob_req.paths.get(i))
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect();
                        GlobResponse {
                            output,
                            paths: None,
                            no_match_patterns,
                        }
                    }
                    Ok(GlobOutcome::Count { paths }) => GlobResponse {
                        output: String::new(),
                        paths: Some(paths),
                        no_match_patterns: Vec::new(),
                    },
                    Err(e) => GlobResponse {
                        output: format!("glob error: {e}"),
                        paths: None,
                        no_match_patterns: Vec::new(),
                    },
                }
            }
            () = async {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 1];
                let _ = buf_reader.read(&mut buf).await;
                cancel_on_disconnect.cancel();
            } => {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    "glob client disconnected — query cancelled",
                );
                span.in_scope(|| {
                    emit_hook_event(
                        tracing::Level::INFO,
                        "cli",
                        &method,
                        Some(&parent_id),
                        "client disconnected",
                        "outgoing hook response",
                    );
                });
                return Ok(());
            }
        };

        let mut payload = serde_json::to_vec(&response)?;

        span.in_scope(|| {
            emit_hook_event(
                tracing::Level::INFO,
                "cli",
                &method,
                Some(&parent_id),
                std::str::from_utf8(&payload).unwrap_or_default(),
                "outgoing hook response",
            );
        });

        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Done editing handoff: prepare ────────────────────────────
    //
    // `pre-tool/editing-stop` is sent by the PreToolUse hook when
    // the agent runs `catenary diagnostics` (internal method name
    // unchanged). Acquires the handoff lock and deposits a snapshot of the
    // batch for the subsequent CLI command — the batch is neither drained nor
    // its flags flipped here (misc 141): delivery, at consume, does that.
    if method == "pre-tool/editing-stop" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        let router = get_or_create_router(&ctx, &session_id, &raw);

        // The PreToolUse hook forwards the real `agent_id` from the host CLI.
        // Snapshot only the requesting agent's batch — reading every agent's
        // bucket would surface a sibling subagent's set, since subagents share
        // the parent's `session_id` and differ only by `agent_id` (bug 37).
        let agent_id = raw
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Derive the EditingManager session key the same way the accumulation
        // path does: the raw `session_id` Option (absent → None), NOT the
        // `"default"`-fallback `session_id` used for the handoff payload and
        // guardrail. The accumulation hook (`pre-tool/editing-state`) keys via
        // `HookRequest.session_id.as_deref()`, so the drain must match exactly
        // or it would look up the wrong key and drain nothing.
        let editing_session = raw.get("session_id").and_then(|v| v.as_str());

        // Acquire the `diagnostics` handoff permit. Blocks only behind another
        // in-flight *diagnostics* handoff (per-key, ADR 014) — never daemon-wide
        // — and holds for milliseconds at most.
        let permit = ctx.handoff.acquire(HandoffKey::Diagnostics).await?;

        // Snapshot the whole of this agent's batch (misc 141): the bare form
        // re-diagnoses every batch file, delivered or not, so `files()` returns
        // all of them. The batch is never mutated here — its `delivered` flags
        // flip only when the consume step's response reaches the client, so a
        // failed or never-connecting `catenary diagnostics` (clap reject,
        // host-killed subprocess) leaves the batch and its gate exactly as they
        // were, ready for a retry. The `editing_session`/`agent_id` key rides the
        // handoff so the consume step flips exactly this bucket.
        let files = router.session.editing.files(editing_session, &agent_id);
        let filtered = router.session.editing.filtered(editing_session, &agent_id);
        let filtered_roots = router
            .session
            .editing
            .filtered_roots(editing_session, &agent_id);

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            agent_id = %agent_id,
            file_count = files.len(),
            filtered,
            "diagnostics: snapshotted batch from EditingManager (flip deferred to delivery)",
        );

        // The editing guardrail is NOT released here (ws37 ticket 02, decision 3;
        // misc 141). The gate is a debt paid by *delivered* diagnostics, so the
        // release moves to the consume step and goes conditional: release iff no
        // undelivered debt remains once the response reaches the client. Prepare
        // only snapshots — a scoped pull that leaves debt keeps the gate armed,
        // and a faulted attempt that never delivers leaves both the batch and the
        // lock intact.

        // Mint the scope UUID for the done-editing IPC execution.
        // This is separate from the prepare handler's own scope_id —
        // the prepare hook is one scope, the IPC execution is another.
        let handoff_parent_id = uuid::Uuid::new_v4().to_string();

        // Stage the file set under `diagnostics` and arm the per-key timeout
        // (cleared if the CLI never connects). Dropping the context — on
        // consume or timeout — releases the permit.
        ctx.handoff.stage(
            HandoffKey::Diagnostics,
            HandoffContext {
                parent_id: handoff_parent_id,
                payload: HandoffPayload::Diagnostics {
                    files,
                    filtered,
                    filtered_roots,
                    session_id: session_id.clone(),
                    editing_session: editing_session.map(str::to_string),
                    agent_id: agent_id.clone(),
                },
                permit,
            },
        );

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            "diagnostics handoff prepared",
        );

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            "{\"status\":\"ok\"}",
            "outgoing hook response",
        );

        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Done editing handoff: run ────────────────────────────────
    //
    // `tool/editing-stop` is sent by the `catenary diagnostics` CLI
    // command (internal method name unchanged). Takes the file list from the
    // handoff slot, runs process_files_batched, and returns diagnostics.
    if method == "tool/editing-stop" {
        // Scoped paths from the consume request (ws37 ticket 02). The CLI
        // resolves relative paths against its cwd before dispatch, so these are
        // absolute. The bare form sends an empty set → the whole batch is
        // re-diagnosed and its flags flipped; a non-empty set means the agent
        // named files to diagnose on demand and pay their debt (misc 141).
        let scoped_files: Vec<PathBuf> = raw
            .get("files")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect()
            })
            .unwrap_or_default();
        let scoped = !scoped_files.is_empty();

        // Take the file list and parent_id from the `diagnostics` slot,
        // releasing the permit immediately. The permit must not be held during
        // the diagnostics pipeline (which may take seconds). Consuming the
        // HandoffContext drops it, releasing the owned semaphore permit.
        let handoff = ctx.handoff.consume(HandoffKey::Diagnostics).map(|h| {
            let HandoffPayload::Diagnostics {
                files,
                filtered,
                filtered_roots,
                session_id,
                editing_session,
                agent_id,
            } = h.payload;
            (
                files,
                filtered,
                filtered_roots,
                session_id,
                editing_session,
                agent_id,
                h.parent_id,
            )
        });

        // The batch is NOT mutated here (misc 141): a `catenary diagnostics` run
        // pays its debt by *delivery*, not on consume, so the `delivered` flags
        // flip only after the response's socket write succeeds (below) — a failed
        // write must leave the flags false and the gate armed. The keys needed to
        // flip the right `(editing_session, agent_id)` bucket ride the handoff and
        // are captured before the borrow-consuming match. Uses the
        // handoff-carried `session_id` (the value prepare staged — the bare
        // consume request itself carries no session identity).
        let flip_keys: Option<(String, Option<String>, String)> = handoff
            .as_ref()
            .map(|h| (h.3.clone(), h.4.clone(), h.5.clone()));

        // Extract scope_id early so we can emit the incoming hook
        // event before running the diagnostics pipeline. This ensures
        // the tool/editing-stop event is the first message in the
        // parent_id group, making it the scope header in the TUI
        // (matching the grep/glob pattern).
        let scope_id = match &handoff {
            Some((_, _, _, _, _, _, parent_id)) => parent_id.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );

        // `dirty` is a status label only (ws37 ticket 01): the CLI exits `0`
        // whether clean or dirty — the clean/dirty distinction lives in the
        // per-file receipt (`output`), where clean files carry `[clean]` and
        // dirty files their diagnostics. Faults (no daemon, IPC/parse failure)
        // are detected CLI-side and exit `2`. `covered` is the count of covered
        // files in the handoff: it lets the CLI print `[no edited files]` for a
        // genuinely empty set (covered == 0, empty receipt).
        let (dirty, output, covered) =
            if let Some((files, filtered, filtered_roots, session_id, _, _, _)) = handoff {
                // A scoped pull diagnoses exactly the named paths; the bare form
                // re-diagnoses the whole batch snapshot (delivered files included).
                // The accumulation-time `filtered` count (files skipped for lack of
                // coverage) applies only to the bare form — a scoped pull names files
                // explicitly, so an uncovered named file renders its own out-of-scope
                // line in the receipt and the accumulation note is suppressed.
                // Clone `scoped_files` for the pipeline — the originals are needed
                // after the match to flip exactly the named files on delivery.
                let (diag_files, filtered, filtered_roots) = if scoped {
                    (scoped_files.clone(), 0, std::collections::BTreeSet::new())
                } else {
                    (files, filtered, filtered_roots)
                };
                let covered = diag_files.len();
                if diag_files.is_empty() {
                    // Nothing covered to diagnose — the note (if any) stands alone.
                    // If edits were made but none had LSP coverage, the editing
                    // session still ended: record an `editing_done` milestone so the
                    // activity ring shows the transition (ticket 08).
                    if filtered > 0 {
                        ctx.primary.record_milestone(
                            crate::state_snapshot::MilestoneKind::EditingDone,
                            "editing done · no covered files",
                            Some(session_id.clone()),
                        );
                    }
                    (
                        false,
                        with_out_of_roots_note(String::new(), filtered, &filtered_roots),
                        covered,
                    )
                } else {
                    // Ephemeral mount (ticket 02): any diagnosed file outside
                    // every mounted root mounts its enclosing project root so the
                    // fresh server can diagnose it — the mounting query pays the
                    // spawn/index. Covers a scoped `catenary diagnostics <path>`
                    // on an out-of-root file, and a bare drain whose debt lives
                    // under a since-expired ephemeral root (re-mount = activity,
                    // refreshing its clock). Runs before the pipeline so
                    // `process_files_batched` sees the file as covered.
                    ensure_ephemeral_mounts(&ctx, &diag_files, Instant::now(), &session_id).await;
                    // Reflect the run on the session board: status → diagnostics
                    // for its duration, then record the result as last_action
                    // (observability ticket 05). Clone the session Arc and drop the
                    // registry lock before the await.
                    let board_session = ctx
                        .sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&session_id)
                        .map(|e| e.router.session.clone());
                    if let Some(s) = &board_session {
                        s.set_diagnostics_in_flight(true);
                    }
                    // Race the diagnostics pipeline against client disconnect. If the
                    // `catenary diagnostics` process is killed mid-settle (e.g. the host
                    // tool-call timeout fires while a server sits in a `$/progress`
                    // bracket), the socket closes, the read below returns EOF, and we
                    // drop the pipeline future instead of leaving a settle wait pinned on
                    // a Busy server (bug 24). Mirrors the grep/glob cancel-on-disconnect
                    // path. The dropped batch self-heals: `open_document_on` sends
                    // `didChange` (not a duplicate `didOpen`) for an already-open doc.
                    let outcome = tokio::select! {
                        outcome = ctx
                            .primary
                            .diagnostics
                            .process_files_batched(&diag_files, Some(&scope_id)) => outcome,
                        () = async {
                            use tokio::io::AsyncReadExt;
                            let mut probe = [0u8; 1];
                            let _ = buf_reader.read(&mut probe).await;
                        } => {
                            if let Some(s) = &board_session {
                                s.set_diagnostics_in_flight(false);
                            }
                            debug!(
                                source = Source::DaemonDispatch.as_str(),
                                session_id = %session_id,
                                "diagnostics client disconnected — pipeline cancelled",
                            );
                            emit_hook_event(
                                tracing::Level::INFO,
                                "cli",
                                &method,
                                Some(&scope_id),
                                "client disconnected",
                                "outgoing hook response",
                            );
                            return Ok(());
                        }
                    };
                    if let Some(s) = &board_session {
                        s.set_diagnostics_in_flight(false);
                        s.set_last_action(format!(
                            "diagnostics: {} errors, {} warnings",
                            outcome.errors, outcome.warnings
                        ));
                    }
                    // Promote the completed run to the activity ring with the result
                    // counts and the covered-file count (ticket 08). Emitted via the
                    // primary's shared snapshot writer so it lands even if the session
                    // already left the registry.
                    ctx.primary.record_milestone(
                        crate::state_snapshot::MilestoneKind::Diagnostics,
                        format!(
                            "{} errors, {} warnings · {} files",
                            outcome.errors,
                            outcome.warnings,
                            diag_files.len()
                        ),
                        Some(session_id.clone()),
                    );
                    // Surface any filtered edits alongside the covered-file results,
                    // so a mixed batch never silently hides the unchecked files.
                    (
                        outcome.dirty,
                        with_out_of_roots_note(outcome.output, filtered, &filtered_roots),
                        covered,
                    )
                }
            } else {
                // Handoff slot was empty — timeout expired or double-consume.
                (
                    false,
                    "diagnostics handoff expired — no files available".to_string(),
                    0,
                )
            };

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&scope_id),
            &output,
            "outgoing hook response",
        );

        // Structured response mirroring the grep/glob JSON envelope. `output` is
        // the rendered per-file receipt the CLI prints verbatim. `status` is a
        // clean/dirty label the CLI no longer maps to an exit code (ws37 ticket
        // 01) — it is retained for telemetry. `covered` (the diagnosed file
        // count — the scoped paths, or the whole batch for a bare pull) lets the
        // CLI print `[no edited files]` for a genuinely empty set (covered == 0,
        // empty receipt; scoped pulls are always non-empty).
        let envelope = serde_json::json!({
            "status": if dirty { "dirty" } else { "clean" },
            "output": output,
            "covered": covered,
        });
        let mut payload = serde_json::to_vec(&envelope)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;

        // The response bytes reached the client — flip the `delivered` flags now
        // (misc 141). `delivered` is a transport fact: a run pays its debt by
        // delivery, so a bare pull marks the whole batch delivered and a scoped
        // pull marks exactly the named files. A failed `write_all` above returned
        // early via `?`, leaving the flags false and the gate armed (the bug-60
        // killed-client shape recovers by re-running — the next bare re-serves the
        // batch, fresh). Once no undelivered debt remains, release the cross-
        // session editing guardrail so another session can claim the root.
        if let Some((session_id, editing_session, agent_id)) = &flip_keys {
            let editing = ctx
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(session_id)
                .map(|e| e.router.session.clone());
            if let Some(session) = editing {
                if scoped {
                    session.editing.mark_delivered(
                        editing_session.as_deref(),
                        agent_id,
                        &scoped_files,
                    );
                } else {
                    session
                        .editing
                        .mark_delivered_all(editing_session.as_deref(), agent_id);
                }
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    agent_id = %agent_id,
                    scoped,
                    "diagnostics: batch flags flipped on delivery (misc 141)",
                );
                if !session
                    .editing
                    .has_undelivered(editing_session.as_deref(), agent_id)
                {
                    ctx.editing_guardrail.release_all(session_id);
                    debug!(
                        source = Source::DaemonDispatch.as_str(),
                        session_id = %session_id,
                        agent_id = %agent_id,
                        "diagnostics: batch fully delivered — editing guardrail released",
                    );
                }
            }
        }

        writer.shutdown().await?;
        return Ok(());
    }

    // ── Root management ──────────────────────────────────────────
    //
    // `tool/roots-add` and `tool/roots-rm` are sent by the CLI commands
    // (`catenary pin`, `catenary unpin`). The PreToolUse hook
    // only bypasses the command filter — no hook-side IPC needed
    // since "hook" is a shared contributor with no session identity.
    //
    // Handled before `get_or_create_router` because root management
    // is a daemon-level concern (RootTracker), not a per-session
    // router concern.
    if method == "tool/roots-add" {
        let scope_id = uuid::Uuid::new_v4().to_string();
        let response = if let Some(path_str) = raw.get("path").and_then(|v| v.as_str()) {
            let path = PathBuf::from(path_str);
            let canonical = path.canonicalize().unwrap_or(path);
            if let Some(ref tracker) = ctx.root_tracker {
                tracker.add_roots("hook", std::slice::from_ref(&canonical));
                // Upgrade an ephemerally-mounted root to pinned (ticket 02): drop
                // its `ephemeral:*` contributor and idle clock so it no longer
                // expires. The `hook` contributor just added keeps the root in the
                // union, so this drops no server — no re-index churn.
                let upgraded = tracker.remove_root(&ephemeral_contributor(&canonical), &canonical);
                if upgraded {
                    ctx.ephemeral_mounts.remove(&canonical);
                }
                let global = tracker.global_roots_rich();
                if let Err(e) = ctx.primary.sync_roots(global).await {
                    debug!(
                        source = Source::DaemonDispatch.as_str(),
                        "root sync after add-root failed: {e}",
                    );
                }
                info!(
                    source = Source::DaemonDispatch.as_str(),
                    path = %canonical.display(),
                    upgraded_from_ephemeral = upgraded,
                    "added root via hook contributor",
                );
            }
            serde_json::json!({"status": "ok", "path": canonical.display().to_string()})
        } else {
            serde_json::json!({"status": "error", "message": "missing path"})
        };

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &response.to_string(),
            "outgoing hook response",
        );

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    if method == "tool/roots-rm" {
        let scope_id = uuid::Uuid::new_v4().to_string();
        let response = if let Some(path_str) = raw.get("path").and_then(|v| v.as_str()) {
            let path = PathBuf::from(path_str);
            let canonical = path.canonicalize().unwrap_or(path);
            if let Some(ref tracker) = ctx.root_tracker {
                let removed = tracker.remove_root("hook", &canonical);
                if removed {
                    let global = tracker.global_roots_rich();
                    if let Err(e) = ctx.primary.sync_roots(global).await {
                        debug!(
                            source = Source::DaemonDispatch.as_str(),
                            "root sync after rm-root failed: {e}",
                        );
                    }
                    info!(
                        source = Source::DaemonDispatch.as_str(),
                        path = %canonical.display(),
                        "removed root from hook contributor",
                    );
                    serde_json::json!({"status": "ok", "path": canonical.display().to_string()})
                } else {
                    serde_json::json!({
                        "status": "not_found",
                        "message": format!(
                            "root not found in hook-managed roots: {}",
                            canonical.display()
                        )
                    })
                }
            } else {
                serde_json::json!({"status": "error", "message": "no root tracker"})
            }
        } else {
            serde_json::json!({"status": "error", "message": "missing path"})
        };

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );
        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &response.to_string(),
            "outgoing hook response",
        );

        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    let router = get_or_create_router(&ctx, &session_id, &raw);

    // Span with session_id so warn!/error! events emitted during
    // hook dispatch route to the correct notification queue.
    let hook_span = tracing::info_span!(
        "hook_dispatch",
        session_id = %session_id,
    );
    let _hook_guard = hook_span.enter();

    // Mint a UUID for this request/response pair.
    let scope_id = uuid::Uuid::new_v4().to_string();

    let request: HookRequest =
        serde_json::from_value(raw.clone()).map_err(|e| anyhow!("Invalid hook request: {e}"))?;

    let mut result = router.dispatch(request);

    // ── Lingering-worktree nag at the parent's Stop (misc 151 D-2) ─────
    //
    // Block-once with the list of the session's worktrees that satisfy ALL of:
    // dir present, root NOT mounted, owning agent NOT running in the stop
    // payload's `background_tasks`, and not yet nagged (once per worktree). A
    // doorbell, not a wall — the `stop_hook_active` retry passes. Fires only on a
    // clean turn: not already blocking on the editing gate, and no pending
    // parent-agent context to deliver first (delivering that keeps `result` an
    // allow this turn; the nag takes the next clean stop). Scoped to the
    // **top-level Stop** (`hook_event_name == "Stop"`) so a leaf subagent's
    // SubagentStop is never held for a sibling's lingering worktree; the
    // mid-tier-parent SubagentStop case is a deferred refinement.
    if method == "post-agent/require-release"
        && raw
            .get("host_payload")
            .and_then(|hp| hp.get("hook_event_name"))
            .and_then(serde_json::Value::as_str)
            == Some("Stop")
        && !raw
            .get("stop_hook_active")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        && result.result.is_none()
        && result.additional_context.is_none()
        && let Some(nag) = lingering_worktree_nag(&ctx, &session_id, &raw)
    {
        result.result = Some(crate::hook::HookResult::Block(nag));
    }

    // ── Subagent worktree teardown at stop (misc 150) ──────────
    //
    // A Claude Code SubagentStop reaches the daemon as this same
    // `post-agent/require-release`. Reap the subagent's worktree root now so its
    // language servers shut down at agent completion, not at the parent's
    // SessionEnd sweep (the reported RAM buildup — the fix).
    //
    // Outcome-gated (maintainer ruling): reap ONLY when the require-release
    // outcome ALLOWS the stop. A `Block` means the agent is NOT stopping — it is
    // about to run `catenary diagnostics` in that very worktree — so the root
    // stays warm (reaping first would tear down the servers the receipt needs).
    // Once the debt is paid, the `stop_hook_active` retry allows the stop and the
    // reap runs.
    //
    // Resolution order (misc 150 hardening): identity/registry first, cwd second —
    // never an exact match on the raw cwd (see `resolve_stop_reap_target`). A pure
    // side effect (decision 029): invisible to the require-release response.
    if method == "post-agent/require-release"
        && let Some(ref tracker) = ctx.root_tracker
        && let Some(hp) = raw.get("host_payload")
        && hp.get("hook_event_name").and_then(|v| v.as_str()) == Some("SubagentStop")
        && !matches!(&result.result, Some(crate::hook::HookResult::Block(_)))
    {
        let agent_id = raw.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
        // Drop the subagent from the board — it has stopped (the stop is allowed;
        // a blocked stop, gated out above, means it is still running).
        ctx.subagents.stop(&session_id, agent_id);
        let cwd = hp.get("cwd").and_then(|v| v.as_str());
        if let Some((contributor, worktree)) =
            resolve_stop_reap_target(&ctx, tracker, &session_id, agent_id, cwd)
        {
            if tracker.has_contributor(&contributor) {
                reap_worktree_root(
                    &ctx,
                    tracker,
                    &session_id,
                    &contributor,
                    &worktree,
                    "subagent stop",
                )
                .await;
            } else {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    agent_id = agent_id,
                    contributor = %contributor,
                    "subagent stop worktree teardown skipped (no live worktree root for identity/cwd)",
                );
            }

            // misc 151 trigger 1: dispose the subagent's worktree in the
            // background (spawn_blocking for the git subprocesses) so the
            // stop-gate response latency is untouched (decision 029). Clean →
            // auto-disposed; dirty → kept and surfaced to the parent (D-1). Runs
            // whether or not the root was still mounted (an idle-reaped worktree
            // still disposes).
            let registry = ctx.worktree_registry.clone();
            let parent_context = ctx.primary.parent_context.clone();
            let sid = session_id.clone();
            let aid = agent_id.to_string();
            let wt = worktree.clone();
            tokio::task::spawn_blocking(move || {
                dispose_worktree_in_background(&registry, &parent_context, &sid, &aid, &wt, false);
            });
        }
    }

    // A hook dispatch carrying an agent identity clears any blocked-on-permission
    // suspension for that agent's worktree root (misc 150) — the human answered
    // the prompt and the agent resumed. (The SubagentStop reap above already
    // dropped the clock, so this is a no-op there.)
    if let Some(aid) = raw
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        ctx.worktree_mounts
            .clear_blocked(&format!("worktree:{session_id}:{aid}"));
    }

    // Edit tracking is a qualifying activity (ticket 02 / misc 150): a `PreToolUse`
    // edit of a file under an ephemeral or worktree root refreshes that root's idle
    // clock (and clears any blocked flag), so an agent actively editing under it
    // never has it expire mid-work. This only *refreshes* — edits never mount (a
    // query does), keeping the edit hook fast.
    if let Some(file_path) = raw.get("file_path").and_then(|v| v.as_str()) {
        let path = Path::new(file_path);
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let now = Instant::now();
        ctx.ephemeral_mounts.touch_covering(&canonical, now);
        ctx.worktree_mounts.touch_covering(&canonical, now);
    }

    let envelope = HookResponseEnvelope {
        result: result.result,
        additional_context: result.additional_context,
    };

    let response = if envelope.result.is_some() || envelope.additional_context.is_some() {
        serde_json::to_string(&envelope)?
    } else {
        String::new()
    };

    // Determine level from outcome and hook category.
    let level = hook_outcome_level(&method, &envelope);

    // Log incoming hook request (deferred — uses outcome-determined level).
    emit_hook_event(
        level,
        &session_id,
        &method,
        Some(&scope_id),
        &raw.to_string(),
        "incoming hook",
    );

    // Log outgoing hook response — same parent_id as request.
    emit_hook_event(
        level,
        &session_id,
        &method,
        Some(&scope_id),
        &response,
        "outgoing hook response",
    );

    writer.write_all(response.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;

    Ok(())
}

#[cfg(unix)]
impl Drop for SessionManager {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.mcp_socket_path);
        let _ = std::fs::remove_file(&self.ipc_socket_path);
    }
}

// ── Bridge proxy ────────────────────────────────────────────────────

/// Maximum number of attempts to connect to the daemon.
const MAX_CONNECT_ATTEMPTS: u32 = 10;

/// Delay between connection retry attempts.
const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Runs the bridge proxy: connect-or-start the daemon, then proxy
/// stdin/stdout to/from the daemon socket.
///
/// Entirely synchronous — no tokio runtime involvement in the data
/// path. This avoids any interaction between the tokio runtime's
/// internal epoll/signal state and the blocking I/O threads.
///
/// # Errors
///
/// Returns an error if the daemon cannot be started, the connection
/// fails, or the daemon closes the connection before stdin.
#[cfg(unix)]
pub fn run_bridge() -> Result<()> {
    // Guard against recursive spawning. If the daemon subprocess
    // somehow enters the bridge path (e.g., the "daemon" arg is lost),
    // this prevents an infinite process chain.
    if std::env::var_os("_CATENARY_BRIDGE").is_some() {
        anyhow::bail!(
            "recursive bridge detected — the daemon subprocess \
             re-entered the bridge path instead of the daemon path"
        );
    }
    let stream = connect_or_start_daemon()?;
    proxy_stdio(stream)
}

/// Connects to a running daemon or starts one.
///
/// Implements the start-or-connect sequence:
/// 1. Try to connect to the MCP socket.
/// 2. If connection fails and a stale socket file exists, remove it.
/// 3. Spawn a daemon process (`catenary daemon`).
/// 4. Retry connection with backoff.
///
/// # Errors
///
/// Returns an error if the daemon cannot be reached after all retry attempts.
#[cfg(unix)]
fn connect_or_start_daemon() -> Result<std::os::unix::net::UnixStream> {
    let mcp_path = mcp_socket_path();
    let mut daemon_spawned = false;

    for attempt in 0..MAX_CONNECT_ATTEMPTS {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(&mcp_path) {
            info!(
                source = Source::DaemonLifecycle.as_str(),
                attempt, "connected to daemon",
            );
            return Ok(stream);
        }

        let last_attempt = attempt == MAX_CONNECT_ATTEMPTS - 1;
        if last_attempt {
            anyhow::bail!(
                "failed to connect to Catenary daemon \
                 after {MAX_CONNECT_ATTEMPTS} attempts ({})",
                mcp_path.display(),
            );
        }

        if !daemon_spawned {
            if mcp_path.exists() {
                let _ = std::fs::remove_file(&mcp_path);
            }
            let ipc_path = socket_path();
            if ipc_path.exists() {
                let _ = std::fs::remove_file(&ipc_path);
            }
            spawn_daemon()?;
            daemon_spawned = true;
        }

        std::thread::sleep(CONNECT_RETRY_DELAY);
    }

    anyhow::bail!(
        "failed to connect to Catenary daemon ({})",
        mcp_path.display(),
    )
}

/// Spawns `catenary daemon` as a detached child process.
///
/// The daemon binds the MCP socket and begins accepting connections.
/// Uses a new process group so the daemon outlives the bridge. Stderr
/// is redirected to `$XDG_STATE_HOME/catenary/daemon.log` so that
/// daemon crashes during initialization are diagnosable from the
/// bridge side (and from integration test failure output).
#[cfg(unix)]
fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("resolve current executable path")?;

    let log_dir = crate::paths::state_dir().join("catenary");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("create daemon log directory: {}", log_dir.display()))?;
    let log_path = log_dir.join("daemon.log");
    let stderr_file = std::fs::File::create(&log_path)
        .with_context(|| format!("create daemon log: {}", log_path.display()))?;

    Command::new(exe)
        .arg("daemon")
        .env("_CATENARY_BRIDGE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .process_group(0)
        .spawn()
        .context("spawn daemon process")?;

    Ok(())
}

/// Proxies stdin/stdout to/from a daemon socket connection.
///
/// Before entering the concurrent proxy, intercepts the first MCP
/// exchange (initialize) to verify the daemon's version matches this
/// bridge's version. On mismatch, returns an error without proxying.
///
/// Uses purely blocking I/O on two threads: one copies stdin to the
/// daemon socket, the other copies the daemon socket to stdout. Both
/// threads share the same socket fd via `try_clone()` — this is safe
/// because both halves stay in blocking mode (no mixed
/// blocking/non-blocking on the same file description). Full-duplex
/// Unix sockets support concurrent read and write from different
/// threads.
///
/// The socket→stdout direction uses a read-write-flush loop because
/// `std::io::Stdout` uses full buffering on pipes. Without explicit
/// flushing, MCP responses sit in the buffer until it fills (8 KB).
///
/// Returns `Ok(())` when stdin closes (host CLI ended the session).
/// Returns `Err` when the daemon connection drops first (unexpected).
///
/// # Errors
///
/// Returns an error if the daemon version does not match, or if the
/// daemon connection closes before stdin (unexpected termination).
#[cfg(unix)]
fn proxy_stdio(stream: std::os::unix::net::UnixStream) -> Result<()> {
    use std::io::{Read, Write};

    // Phase 1: Version handshake (blocking, sequential).
    // Intercepts the first MCP exchange (initialize) to verify that the
    // daemon version matches this bridge. On mismatch, the handshake
    // sends a catenary/version-mismatch notification to the daemon and
    // returns Err.
    {
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        version_handshake(&mut stdin, &stream, &mut stdout)?;
    }

    // Phase 2: Concurrent byte proxy for remaining messages.
    let writer = stream.try_clone().context("clone daemon socket")?;
    let reader = stream;

    // stdin → socket: dedicated thread, blocks until stdin EOF.
    let stdin_thread = std::thread::spawn(move || -> Result<()> {
        let mut stdin = std::io::stdin().lock();
        let mut w = writer;
        std::io::copy(&mut stdin, &mut w).context("proxy stdin to socket")?;
        let _ = w.shutdown(std::net::Shutdown::Write);
        Ok(())
    });

    // socket → stdout: runs on calling thread, blocks until socket EOF.
    let mut stdout = std::io::stdout().lock();
    let mut buf = vec![0u8; 8192];
    let mut r = reader;
    let stdout_result: Result<()> = loop {
        match r.read(&mut buf) {
            Ok(0) => break Err(anyhow::anyhow!("daemon connection closed unexpectedly")),
            Ok(n) => {
                if let Err(e) = stdout.write_all(&buf[..n]) {
                    break Err(anyhow::Error::from(e).context("write to stdout"));
                }
                if let Err(e) = stdout.flush() {
                    break Err(anyhow::Error::from(e).context("flush stdout"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                // stdout pipe closed (host killed the process).
                break Ok(());
            }
            Err(e) => break Err(anyhow::Error::from(e).context("read from daemon")),
        }
    };

    // If we got here, the socket→stdout loop ended. Either the daemon
    // died (Err) or stdout pipe broke (Ok). The stdin thread may still
    // be blocked; don't join it — the process is exiting.
    //
    // If stdin closed first, the stdin thread already exited and the
    // daemon will close the connection → we exit via the read loop above.
    drop(stdin_thread);

    stdout_result
}

/// Intercepts the MCP initialize handshake to verify daemon version.
///
/// Reads the initialize request from `client`, forwards it to `socket`,
/// reads the response, and checks `serverInfo.version` against this
/// bridge's version ([`CATENARY_VERSION`](env!("CATENARY_VERSION"))).
/// On match, forwards the response to `output`. On mismatch, sends a
/// `catenary/version-mismatch` notification to the daemon and returns
/// an error.
///
/// Generic over reader/writer for testability — `proxy_stdio` passes
/// stdin/stdout, tests pass in-memory buffers.
#[cfg(unix)]
fn version_handshake<R: std::io::BufRead, W: std::io::Write>(
    client: &mut R,
    socket: &std::os::unix::net::UnixStream,
    output: &mut W,
) -> Result<()> {
    use std::io::Write;

    // Read the initialize request from the client (one JSON-RPC line).
    let mut init_line = String::new();
    client
        .read_line(&mut init_line)
        .context("read initialize request from client")?;

    // Forward to daemon.
    (&*socket)
        .write_all(init_line.as_bytes())
        .context("forward initialize request to daemon")?;
    (&*socket).flush()?;

    // Read the initialize response from daemon.
    // Byte-by-byte to avoid consuming data beyond the line boundary,
    // which would be lost to the subsequent concurrent byte proxy.
    let response_line = read_json_line(socket).context("read initialize response from daemon")?;

    // Parse and check version.
    let response: serde_json::Value =
        serde_json::from_str(response_line.trim()).context("parse initialize response")?;

    let daemon_version = response
        .pointer("/result/serverInfo/version")
        .and_then(|v| v.as_str());

    let bridge_version = env!("CATENARY_VERSION");

    match daemon_version {
        Some(dv) if dv == bridge_version => {}
        Some(dv) => {
            // Notify daemon of the mismatch before disconnecting so it
            // can surface the event via the notification sink.
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "catenary/version-mismatch",
                "params": { "bridgeVersion": bridge_version }
            });
            if let Ok(line) = serde_json::to_string(&notification) {
                let _ = (&*socket).write_all(line.as_bytes());
                let _ = (&*socket).write_all(b"\n");
                let _ = (&*socket).flush();
            }

            anyhow::bail!(
                "Catenary version mismatch: daemon is v{dv}, \
                 bridge is v{bridge_version}. Run 'catenary stop' and retry."
            );
        }
        None => {
            anyhow::bail!(
                "daemon did not report a version in serverInfo — \
                 not a Catenary daemon or a bug"
            );
        }
    }

    // Version matches — forward the response to the client.
    output
        .write_all(response_line.as_bytes())
        .context("forward initialize response to client")?;
    output.flush()?;

    Ok(())
}

/// Reads a single newline-terminated line from a socket without buffering.
///
/// Reads byte-by-byte so that no data beyond the line boundary is consumed
/// from the kernel's receive buffer, which is shared across all handles to
/// the same file descriptor.
#[cfg(unix)]
fn read_json_line(socket: &std::os::unix::net::UnixStream) -> Result<String> {
    use std::io::Read;

    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    loop {
        (&*socket)
            .read_exact(&mut byte)
            .context("read byte from socket")?;
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(buf).context("response is not valid UTF-8")
}

#[cfg(test)]
#[cfg(unix)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "tests intentionally hold SessionManager alive for socket lifetime"
)]
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`{root}Internal` companion templates are placeholders, not format args"
)]
mod tests {
    use super::*;
    use crate::logging::LoggingServer;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    /// Create an MCP socket path inside a tempdir.
    fn mcp_socket_in(dir: &Path) -> PathBuf {
        dir.join("catenary").join("catenary-mcp.sock")
    }

    /// Create an IPC socket path inside a tempdir.
    fn ipc_socket_in(dir: &Path) -> PathBuf {
        dir.join("catenary").join("catenary.sock")
    }

    /// Bind a `SessionManager` with both sockets in a tempdir.
    fn bind_in(dir: &Path) -> SessionManager {
        SessionManager::bind_at(
            &mcp_socket_in(dir),
            &ipc_socket_in(dir),
            LoggingServer::new(),
        )
        .expect("bind")
    }

    // ── Tracing capture layer ──────────────────────────────────────

    /// Minimal tracing layer that captures `source` field values.
    struct CaptureLayer {
        sources: Arc<Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(Option<String>);

            impl tracing::field::Visit for Visitor {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "source" {
                        self.0 = Some(value.to_string());
                    }
                }

                fn record_debug(
                    &mut self,
                    _field: &tracing::field::Field,
                    _value: &dyn std::fmt::Debug,
                ) {
                }
            }

            let mut v = Visitor(None);
            event.record(&mut v);
            if let Some(src) = v.0
                && let Ok(mut sources) = self.sources.lock()
            {
                sources.push(src);
            }
        }
    }

    // ── Tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn bind_creates_socket_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let _manager = bind_in(dir.path());

        assert!(mcp_path.exists(), "MCP socket file should exist after bind");
    }

    #[tokio::test]
    async fn accept_connection() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(manager.connection_count(), 1);

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn multiple_connections() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let streams: Vec<_> = {
            let mut v = Vec::new();
            for _ in 0..3 {
                v.push(
                    tokio::net::UnixStream::connect(&mcp_path)
                        .await
                        .expect("connect"),
                );
            }
            v
        };

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(manager.connection_count(), 3);

        drop(streams);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn drop_removes_socket() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let manager = bind_in(dir.path());
        assert!(mcp_path.exists(), "MCP socket should exist before drop");
        assert!(ipc_path.exists(), "IPC socket should exist before drop");

        drop(manager);

        assert!(
            !mcp_path.exists(),
            "MCP socket should be removed after drop"
        );
        assert!(
            !ipc_path.exists(),
            "IPC socket should be removed after drop"
        );
    }

    #[tokio::test]
    async fn bind_fails_if_mcp_socket_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        // Create a regular file at the MCP socket path.
        std::fs::create_dir_all(mcp_path.parent().expect("parent")).expect("create dir");
        std::fs::write(&mcp_path, b"").expect("create file");

        let result = SessionManager::bind_at(&mcp_path, &ipc_path, LoggingServer::new());
        assert!(
            result.is_err(),
            "bind should fail when MCP socket already exists"
        );
    }

    #[tokio::test]
    async fn bind_fails_if_ipc_socket_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        // Create a regular file at the IPC socket path.
        std::fs::create_dir_all(ipc_path.parent().expect("parent")).expect("create dir");
        std::fs::write(&ipc_path, b"").expect("create file");

        let result = SessionManager::bind_at(&mcp_path, &ipc_path, LoggingServer::new());
        assert!(
            result.is_err(),
            "bind should fail when IPC socket already exists"
        );
    }

    #[tokio::test]
    async fn startup_tracing_event() {
        let sources = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            sources: Arc::clone(&sources),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let _manager = tracing::subscriber::with_default(subscriber, || {
            SessionManager::bind_at(&mcp_path, &ipc_path, LoggingServer::new())
        })
        .expect("bind");

        let captured = sources.lock().expect("lock").clone();
        assert!(
            captured.contains(&"daemon.lifecycle".to_string()),
            "should emit daemon.lifecycle event, got: {captured:?}",
        );
    }

    // ── Bridge proxy tests ────────────────────────────────────────────

    #[tokio::test]
    async fn bridge_cleans_stale_socket() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        // Create a stale socket file (regular file, nobody listening).
        std::fs::create_dir_all(mcp_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&mcp_path, b"stale").expect("create stale file");

        // Connect should fail on a regular file.
        let result = tokio::net::UnixStream::connect(&mcp_path).await;
        assert!(result.is_err());

        // Clean stale file (what connect_or_start_daemon does).
        std::fs::remove_file(&mcp_path).expect("remove stale");
        assert!(!mcp_path.exists());

        // Now bind succeeds.
        let _manager = bind_in(dir.path());
        assert!(mcp_path.exists());
    }

    #[tokio::test]
    async fn bridge_proxies_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("proxy.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");

        let client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connect");
        let (mut server, _) = listener.accept().await.expect("accept");

        let (mut client_read, mut client_write) = client.into_split();

        // Client → server direction.
        client_write.write_all(b"hello").await.expect("write");
        client_write.shutdown().await.expect("shutdown write");

        let mut buf = vec![0u8; 5];
        server.read_exact(&mut buf).await.expect("server read");
        assert_eq!(&buf, b"hello");

        // Server → client direction.
        server.write_all(b"world").await.expect("server write");
        server.shutdown().await.expect("shutdown server");

        let mut response = Vec::new();
        client_read
            .read_to_end(&mut response)
            .await
            .expect("client read");
        assert_eq!(&response, b"world");
    }

    #[tokio::test]
    async fn bridge_exits_on_daemon_death() {
        use tokio::io::AsyncReadExt;

        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("death.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");

        let client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connect");
        let (server, _) = listener.accept().await.expect("accept");

        // Simulate daemon death.
        drop(server);
        drop(listener);

        let mut buf = Vec::new();
        let mut client = client;
        let n = client
            .read_to_end(&mut buf)
            .await
            .expect("read after daemon death");
        assert_eq!(n, 0, "bridge should see EOF when daemon dies");
    }

    #[tokio::test]
    async fn bridge_handles_race() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Spawn 5 connections concurrently. Hold them alive so the
        // per-connection MCP tasks don't exit (EOF → count decrement).
        let mut handles = Vec::new();
        for _ in 0..5 {
            let p = mcp_path.clone();
            handles.push(tokio::spawn(async move {
                tokio::net::UnixStream::connect(&p).await
            }));
        }

        let mut streams = Vec::new();
        for handle in handles {
            streams.push(handle.await.expect("task").expect("connect"));
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 5);

        drop(streams);
        shutdown.cancel();
    }

    // ── IPC socket tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn ipc_socket_created_on_bind() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let _manager = bind_in(dir.path());

        assert!(ipc_path.exists(), "IPC socket file should exist after bind");
    }

    #[tokio::test]
    async fn ipc_connection_accepted() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _stream = tokio::net::UnixStream::connect(&ipc_path)
            .await
            .expect("connect to IPC socket");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn ipc_and_mcp_sockets_independent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Connect to both sockets simultaneously.
        let (mcp_result, ipc_result) = tokio::join!(
            tokio::net::UnixStream::connect(&mcp_path),
            tokio::net::UnixStream::connect(&ipc_path),
        );

        let mcp_stream = mcp_result.expect("connect to MCP socket");
        let _ipc_stream = ipc_result.expect("connect to IPC socket");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Only MCP connections are tracked.
        assert_eq!(manager.connection_count(), 1);

        drop(mcp_stream);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn hook_passthrough_response() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = tokio::net::UnixStream::connect(&ipc_path)
            .await
            .expect("connect");
        let (reader, mut writer) = stream.into_split();

        // Send a hook request.
        let request = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "agent_id": "",
            "session_id": "test-session"
        });
        let mut payload = serde_json::to_string(&request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        // Read the passthrough response (empty line = allow).
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");
        assert_eq!(line.trim(), "", "passthrough should return empty response");

        shutdown.cancel();
    }

    // ── Per-connection MCP stack tests ────────────────────────────────

    /// Helper: send JSON line, read JSON line response over a std stream.
    fn mcp_roundtrip(
        stream: &std::os::unix::net::UnixStream,
        request: &serde_json::Value,
    ) -> serde_json::Value {
        use std::io::{BufRead, Write};
        let mut buf_writer = std::io::BufWriter::new(stream.try_clone().expect("clone"));
        let line = serde_json::to_string(request).expect("serialize");
        writeln!(buf_writer, "{line}").expect("write");
        buf_writer.flush().expect("flush");

        let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
        let mut response_line = String::new();
        reader.read_line(&mut response_line).expect("read");
        serde_json::from_str(response_line.trim()).expect("parse response")
    }

    #[tokio::test]
    async fn transport_agnostic_mcp() {
        use std::io::{BufRead, Write};

        // Run McpServer with a Unix stream pair (in-process, no filesystem).
        let (server_stream, client_stream) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");
        let reader = server_stream.try_clone().expect("clone for reader");
        let writer = server_stream;

        let logging = LoggingServer::new();
        let handle = std::thread::spawn(move || {
            let mut mcp = McpServer::new(logging);
            mcp.run(reader, writer)
        });

        // Client side: send initialize.
        let mut client_writer = std::io::BufWriter::new(client_stream.try_clone().expect("clone"));
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        });
        let line = serde_json::to_string(&init).expect("serialize");
        writeln!(client_writer, "{line}").expect("write");
        client_writer.flush().expect("flush");

        // Read response.
        let mut client_reader = std::io::BufReader::new(&client_stream);
        let mut response_line = String::new();
        client_reader.read_line(&mut response_line).expect("read");
        let response: serde_json::Value =
            serde_json::from_str(response_line.trim()).expect("parse");

        assert!(response.get("result").is_some(), "should have result");
        assert_eq!(response["result"]["serverInfo"]["name"], "catenary");

        // Close client to signal EOF → server exits.
        drop(client_writer);
        drop(client_stream);
        handle.join().expect("server thread").expect("server run");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_connection_mcp_initialize() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Connect and send MCP initialize.
        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set timeout");

        let response = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            }),
        );

        assert!(
            response.get("result").is_some(),
            "expected result in initialize response, got: {response}",
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "catenary");

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_connection_tools_list_returns_method_not_found() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set timeout");

        // Initialize first.
        let _ = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            }),
        );

        // tools/list should return method-not-found (no tools on MCP).
        let response = mcp_roundtrip(
            &stream,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }),
        );

        assert!(
            response.get("error").is_some(),
            "tools/list should return error, got: {response}",
        );

        drop(stream);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_cleanup_on_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let stream = std::os::unix::net::UnixStream::connect(&mcp_path).expect("connect");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 1, "should have 1 connection");

        // Disconnect.
        drop(stream);

        // Wait for cleanup (MCP server detects EOF and task exits).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            manager.connection_count(),
            0,
            "connection should be cleaned up after disconnect"
        );

        shutdown.cancel();
    }

    // ── Lifecycle tests ─────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_on_last_disconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Connect one client.
        let stream = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 1);

        // Disconnect — last client gone, accept_loop should exit.
        drop(stream);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");

        // Sockets removed so new bridges start a fresh daemon.
        assert!(
            !mcp_path.exists(),
            "MCP socket should be removed after shutdown",
        );
        assert!(
            !ipc_path.exists(),
            "hook socket should be removed after shutdown",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_with_multiple_clients() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Connect two clients.
        let stream1 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 1");
        let stream2 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 2");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.connection_count(), 2);

        // Disconnect first — daemon should stay alive.
        drop(stream1);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "accept_loop should still be running with one client",
        );

        // Disconnect second — daemon should exit.
        drop(stream2);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");
    }

    #[tokio::test]
    async fn stop_via_ipc_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Send shutdown via IPC socket.
        let stream = tokio::net::UnixStream::connect(&ipc_path)
            .await
            .expect("connect to IPC socket");
        let (reader, mut writer) = stream.into_split();

        let request = serde_json::json!({"method": "tool/shutdown"});
        let mut payload = serde_json::to_string(&request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        // Read the ack response.
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");
        assert!(
            line.contains("ok"),
            "should receive ok response, got: {line}",
        );

        // With no live MCP connection, the ack reports zero — the CLI stays
        // quiet (no strand warning).
        let ack: serde_json::Value = serde_json::from_str(line.trim()).expect("parse ack");
        assert_eq!(
            ack.get("connections").and_then(serde_json::Value::as_u64),
            Some(0),
            "ack should report zero connections, got: {line}",
        );

        // accept_loop should exit.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");

        // Sockets removed so new bridges start a fresh daemon.
        assert!(
            !mcp_path.exists(),
            "MCP socket should be removed after stop",
        );
        assert!(
            !ipc_path.exists(),
            "IPC socket should be removed after stop",
        );
    }

    /// The shutdown ack reports the live MCP connection count so `catenary
    /// stop` can warn that those sessions just lost tooling. Here one bridge
    /// is "connected" (count bumped directly), so the ack must report 1.
    #[tokio::test]
    async fn stop_ack_reports_live_connection_count() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));

        // Simulate a live MCP connection (the accept loop bumps this on every
        // accepted MCP stream).
        manager.connection_count.fetch_add(1, Ordering::Relaxed);

        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        let stream = tokio::net::UnixStream::connect(&ipc_path)
            .await
            .expect("connect to IPC socket");
        let (reader, mut writer) = stream.into_split();

        let request = serde_json::json!({"method": "tool/shutdown"});
        let mut payload = serde_json::to_string(&request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");

        let ack: serde_json::Value = serde_json::from_str(line.trim()).expect("parse ack");
        assert_eq!(
            ack.get("connections").and_then(serde_json::Value::as_u64),
            Some(1),
            "ack should report the live connection count, got: {line}",
        );

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");
        assert!(result.is_ok(), "accept_loop should return Ok");
    }

    #[tokio::test]
    async fn version_via_ipc_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        // Session-less manager — exercises the `handle_hook_connection` path,
        // the transport-only daemon (e.g. `lsp = false`) still answers version.
        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        let stream = tokio::net::UnixStream::connect(&ipc_path)
            .await
            .expect("connect to IPC socket");
        let (reader, mut writer) = stream.into_split();

        let request = serde_json::json!({"method": "tool/version"});
        let mut payload = serde_json::to_string(&request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");

        let response: serde_json::Value =
            serde_json::from_str(line.trim()).expect("version response json");
        assert_eq!(
            response.get("version").and_then(serde_json::Value::as_str),
            Some(env!("CATENARY_VERSION")),
            "tool/version returns the daemon's CATENARY_VERSION",
        );

        manager.shutdown_token().cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_via_ipc_socket_with_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        // Session-aware manager — exercises the `handle_hook_dispatch` path,
        // the shape a real daemon serves.
        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let resp = hook_roundtrip(&ipc_path, &serde_json::json!({"method": "tool/version"})).await;
        let response: serde_json::Value =
            serde_json::from_str(resp.trim()).expect("version response json");
        assert_eq!(
            response.get("version").and_then(serde_json::Value::as_str),
            Some(env!("CATENARY_VERSION")),
            "tool/version returns the daemon's CATENARY_VERSION",
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    // ── Antigravity PreInvocation first-sighting (teaching-surface ticket 03) ─

    #[test]
    fn first_sightings_see_is_true_once_per_conversation() {
        // The core dedup: the first sighting of a conversation injects (`true`),
        // every later one does not (`false`), and a distinct conversation is its
        // own first sighting.
        let ledger = FirstSightings::new();
        assert!(ledger.see("conv-a"), "first sighting of conv-a injects");
        assert!(
            !ledger.see("conv-a"),
            "second sighting of conv-a injects nothing"
        );
        assert!(!ledger.see("conv-a"), "and every later one, too");
        assert!(
            ledger.see("conv-b"),
            "a different conversation is its own first sighting"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_invocation_first_sighting_injects_once_over_ipc() {
        // End-to-end over the daemon's `handle_hook_dispatch` path: the first
        // `pre-invocation/first-sighting` for a conversationId answers
        // `inject: true`, the second answers `inject: false`, and a fresh
        // conversationId answers `inject: true` again — so the CLI injects the
        // persisted teaching `userMessage` exactly once per conversation.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let req = |id: &str| {
            serde_json::json!({
                "method": "pre-invocation/first-sighting",
                "format": "antigravity",
                "session_id": id,
            })
        };
        let inject = |resp: &str| -> bool {
            serde_json::from_str::<serde_json::Value>(resp.trim())
                .expect("first-sighting response json")
                .get("inject")
                .and_then(serde_json::Value::as_bool)
                .expect("inject field")
        };

        assert!(
            inject(&hook_roundtrip(&ipc_path, &req("conv-1")).await),
            "first sighting of conv-1 injects",
        );
        assert!(
            !inject(&hook_roundtrip(&ipc_path, &req("conv-1")).await),
            "second sighting of conv-1 injects nothing",
        );
        assert!(
            inject(&hook_roundtrip(&ipc_path, &req("conv-2")).await),
            "a distinct conversation is its own first sighting",
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn shutdown_token_exits_accept_loop() {
        let dir = tempfile::tempdir().expect("create tempdir");

        let manager = Arc::new(bind_in(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Cancel the token directly (simulates signal handling).
        shutdown.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");
    }

    // ── Session state tests ─────────────────────────────────────────────

    /// Create a `SessionManager` with a real `Session` for hook dispatch tests.
    fn bind_with_session(dir: &Path) -> SessionManager {
        bind_with_session_roots(dir, vec![])
    }

    /// Like [`bind_with_session`] but registers `roots` as workspace roots, so
    /// files under them have LSP coverage (tiers 1–2) for editing accumulation.
    fn bind_with_session_roots(dir: &Path, roots: Vec<PathBuf>) -> SessionManager {
        let logging = LoggingServer::new();
        let runtime = tokio::runtime::Handle::current();
        let instance_id: Arc<str> = "daemon".into();
        let parent_context = crate::bridge::ParentContextQueue::new();
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default_with_classification(),
            roots,
            logging.clone(),
            instance_id,
            runtime,
            parent_context,
            None,
        ));

        SessionManager::bind_at(&mcp_socket_in(dir), &ipc_socket_in(dir), logging)
            .expect("bind")
            .with_session(session)
    }

    #[test]
    fn subagent_registry_start_stop_and_clear() {
        let reg = SubagentRegistry::new();
        reg.start("sess-1", "agent-a", "2026-06-08T13:10:00.000Z".to_string());
        reg.start("sess-1", "agent-b", "2026-06-08T13:11:00.000Z".to_string());
        // Idempotent per agent id — a duplicate start is ignored.
        reg.start("sess-1", "agent-a", "2026-06-08T13:12:00.000Z".to_string());
        // A blank agent id (path-keyed session) records nothing.
        reg.start("sess-1", "", "2026-06-08T13:13:00.000Z".to_string());

        let live = reg.for_session("sess-1");
        assert_eq!(live.len(), 2, "two distinct subagents, start-time sorted");
        assert_eq!(live[0].id, "agent-a");
        assert_eq!(live[1].id, "agent-b");
        assert!(reg.for_session("other").is_empty(), "no cross-session leak");

        reg.stop("sess-1", "agent-a");
        assert_eq!(reg.for_session("sess-1"), vec![live[1].clone()]);

        reg.clear_session("sess-1");
        assert!(
            reg.for_session("sess-1").is_empty(),
            "session sweep drops all"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_board_builds_rich_entries() {
        use crate::state_snapshot::{SessionBoard, SessionStatus};

        let instance_id: Arc<str> = "sess-1".into();
        let parent_context = crate::bridge::ParentContextQueue::new();
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default(),
            vec![],
            LoggingServer::new(),
            instance_id.clone(),
            tokio::runtime::Handle::current(),
            parent_context,
            None,
        ));
        let router = Arc::new(HookRouter::new(
            session.clone(),
            instance_id,
            "sess-1".to_string(),
        ));

        let sessions = Arc::new(std::sync::Mutex::new(HashMap::new()));
        sessions.lock().expect("lock").insert(
            "sess-1".to_string(),
            SessionEntry {
                router,
                meta: SessionMeta {
                    client_name: Some("claude".to_string()),
                    started_at: "2026-06-08T13:10:00.000Z".to_string(),
                    roots: vec!["/p/A".to_string(), "/p/B".to_string()],
                },
            },
        );
        let board = SessionBoardImpl {
            sessions,
            subagents: SubagentRegistry::new(),
        };

        // Idle to start: no editing accumulator, no last_action. Client name
        // from the payload `format`; version unknown (omitted).
        let entries = board.sessions();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.id, "sess-1");
        assert_eq!(e.client.name, "claude");
        assert!(e.client.version.is_none());
        assert_eq!(e.roots, vec!["/p/A".to_string(), "/p/B".to_string()]);
        assert_eq!(e.status, SessionStatus::Idle);
        assert!(e.last_action.is_none());
        // `last_seen` (recency) is read live off the session — initialized to a
        // real ISO timestamp at construction, distinct from `last_action`
        // (ticket 05a).
        assert!(
            !e.last_seen.is_empty(),
            "last_seen is a non-empty ISO string"
        );

        // The gate is the truth source (tui-rework 14, item 1): an accumulator
        // with no undelivered covered file is `working`, not `editing` — only
        // an armed gate reads as `editing`.
        session
            .editing
            .start_editing(Some("sess-1"), "")
            .expect("start editing");
        assert_eq!(board.sessions()[0].status, SessionStatus::Working);

        // A covered edit arms the gate → `editing`.
        session.editing.record_covered_edit(
            Some("sess-1"),
            "",
            std::path::PathBuf::from("/p/A/src/lib.rs"),
        );
        assert_eq!(board.sessions()[0].status, SessionStatus::Editing);

        // Full delivery (a bare `catenary diagnostics` receipt) pays the gate:
        // the batch is retained but fully diagnosed → back to `working`.
        session.editing.mark_delivered_all(Some("sess-1"), "");
        assert_eq!(board.sessions()[0].status, SessionStatus::Working);

        // `done_editing` drops the accumulator → `idle`.
        session.editing.done_editing(Some("sess-1"), "");
        assert_eq!(board.sessions()[0].status, SessionStatus::Idle);

        // Re-arm for the diagnostics-in-flight leg below.
        session
            .editing
            .start_editing(Some("sess-1"), "")
            .expect("restart editing");

        // An in-flight diagnostics run shows `diagnostics`; completion records
        // last_action with the counts.
        session.set_diagnostics_in_flight(true);
        assert_eq!(board.sessions()[0].status, SessionStatus::Diagnostics);
        session.set_diagnostics_in_flight(false);
        session.set_last_action("diagnostics: 2 errors, 1 warnings");
        let after = board.sessions();
        let la = after[0].last_action.as_ref().expect("last_action set");
        assert_eq!(la.summary, "diagnostics: 2 errors, 1 warnings");
        assert!(!la.at.is_empty(), "last_action carries a timestamp");

        // A subagent's board entry is enriched with its own per-(session, agent)
        // batch status (tui-rework 14, item 3): fresh → idle, covered edit →
        // editing, delivered → working.
        board
            .subagents
            .start("sess-1", "agent-a", "2026-06-08T13:11:00.000Z".to_string());
        assert_eq!(board.sessions()[0].subagents[0].status, SessionStatus::Idle);
        session
            .editing
            .start_editing(Some("sess-1"), "agent-a")
            .expect("subagent editing");
        session.editing.record_covered_edit(
            Some("sess-1"),
            "agent-a",
            std::path::PathBuf::from("/p/A/src/sub.rs"),
        );
        assert_eq!(
            board.sessions()[0].subagents[0].status,
            SessionStatus::Editing
        );
        session
            .editing
            .mark_delivered_all(Some("sess-1"), "agent-a");
        assert_eq!(
            board.sessions()[0].subagents[0].status,
            SessionStatus::Working
        );
    }

    /// Send a hook JSON request and read the response line.
    async fn hook_roundtrip(ipc_path: &Path, request: &serde_json::Value) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stream = tokio::net::UnixStream::connect(ipc_path)
            .await
            .expect("connect to IPC socket");
        let (reader, mut writer) = stream.into_split();

        let mut payload = serde_json::to_string(request).expect("serialize");
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.expect("write");

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.expect("read");
        line
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_hook_creates_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        assert_eq!(manager.session_count(), 0, "no sessions initially");

        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send a hook with session_id = "abc".
        let request = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Read",
            "session_id": "abc"
        });
        let _ = hook_roundtrip(&ipc_path, &request).await;

        assert_eq!(
            manager.session_count(),
            1,
            "session 'abc' should exist in registry"
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_hook_routes_to_correct_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send hooks with two different session_ids.
        let req_a = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Read",
            "session_id": "session-a"
        });
        let req_b = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Read",
            "session_id": "session-b"
        });
        let _ = hook_roundtrip(&ipc_path, &req_a).await;
        let _ = hook_roundtrip(&ipc_path, &req_b).await;

        assert_eq!(
            manager.session_count(),
            2,
            "should have two independent sessions"
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_last_seen_advances_on_every_dispatch() {
        use crate::state_snapshot::{SessionBoard, SessionStatus};

        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // A PreToolUse `Read` creates the session and stamps `last_seen`. A
        // `Read` is a non-action: it leaves status idle and records no
        // `last_action`.
        let read_req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Read",
            "agent_id": "",
            "session_id": "live",
        });
        let _ = hook_roundtrip(&ipc_path, &read_req).await;

        // A board over the live registry, plus the session handle, to read the
        // rich entry and simulate an action boundary.
        let (board, session) = {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let session = ctx
                .sessions
                .lock()
                .expect("lock")
                .get("live")
                .expect("session 'live' exists")
                .router
                .session
                .clone();
            (
                SessionBoardImpl {
                    sessions: ctx.sessions.clone(),
                    subagents: ctx.subagents.clone(),
                },
                session,
            )
        };

        let first = board.sessions();
        assert_eq!(first.len(), 1);
        let entry = &first[0];
        assert_eq!(
            entry.status,
            SessionStatus::Idle,
            "a Read leaves status idle"
        );
        assert!(entry.last_action.is_none(), "a Read records no last_action");
        assert!(
            !entry.last_seen.is_empty(),
            "last_seen serialized as an ISO string"
        );
        let seen_after_first = entry.last_seen.clone();

        // Simulate an earlier meaningful action (an edit): sets `last_action.at`.
        session.set_last_action("edited src/db.rs");
        let action_at = board.sessions()[0]
            .last_action
            .as_ref()
            .expect("last_action set")
            .at
            .clone();

        // Distinct millisecond so the next bump is observable (millis precision).
        tokio::time::sleep(Duration::from_millis(2)).await;

        // A later `Read` — status and last_action unchanged — still bumps
        // `last_seen`.
        let _ = hook_roundtrip(&ipc_path, &read_req).await;
        let after = board.sessions();
        let entry = &after[0];
        assert!(
            entry.last_seen > seen_after_first,
            "last_seen advances on a later hook dispatch ({seen_after_first} -> {})",
            entry.last_seen,
        );
        assert_eq!(
            entry
                .last_action
                .as_ref()
                .expect("last_action retained")
                .summary,
            "edited src/db.rs",
            "a non-action Read does not change last_action",
        );
        // last_seen (latest Read) ≠ last_action.at (earlier edit): recency moved
        // past the last meaningful action.
        assert!(
            entry.last_seen > action_at,
            "last_seen ({}) should be later than last_action.at ({action_at})",
            entry.last_seen,
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_editing_per_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Session A enters editing mode and accumulates a covered file. The
        // boundary block gates on a non-empty *covered* tracked set, not the
        // mode bit, so a file must be tracked for the gate to fire. This
        // harness configures no covered root, so accumulate via the session's
        // editing manager directly.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "session-a"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router_a = Arc::clone(&sessions.get("session-a").expect("session-a").router);
            drop(sessions);
            router_a.session.editing.record_covered_edit(
                Some("session-a"),
                "",
                std::path::PathBuf::from("/src/main.rs"),
            );
        }

        // Session A has a covered set pending: a non-filesystem Bash command is
        // gated (the agent must run `catenary diagnostics` first).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Bash",
            "command": "cargo build",
            "agent_id": "",
            "session_id": "session-a"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        let envelope: crate::hook::HookResponseEnvelope =
            serde_json::from_str(line.trim()).expect("parse response");
        assert!(
            matches!(envelope.result, Some(crate::hook::HookResult::Deny(_))),
            "session A (covered set pending) should gate non-filesystem Bash, got: {envelope:?}"
        );

        // Session B never entered editing mode: the same command is allowed,
        // proving editing state does not leak across sessions.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Bash",
            "command": "cargo build",
            "agent_id": "",
            "session_id": "session-b"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert_eq!(
            line.trim(),
            "",
            "session B (not editing) should allow Bash — editing state is per-session"
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_state_subagent_passthrough() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Hook with non-empty agent_id should pass through without
        // triggering editing enforcement.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Read",
            "agent_id": "sub-agent-1",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert_eq!(
            line.trim(),
            "",
            "subagent hook should pass through (empty response)"
        );

        shutdown.cancel();
    }

    // ── Done editing handoff tests ────────────────────────────────────

    /// Send a hook JSON request and read all response data (may be
    /// multi-line, unlike `hook_roundtrip` which reads a single line).
    ///
    /// Does NOT shutdown the write side after sending. `tool/editing-stop`
    /// races its diagnostics pipeline against client disconnect (bug 24); a
    /// write-shutdown reads as EOF on the daemon side and would trip the
    /// disconnect branch before the response is sent. EOF still arrives — the
    /// daemon shuts down its write half after every response. Mirrors the
    /// production client, which keeps the write half open while reading.
    async fn hook_roundtrip_full(ipc_path: &Path, request: &serde_json::Value) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::UnixStream::connect(ipc_path)
            .await
            .expect("connect to IPC socket");

        let mut payload = serde_json::to_string(request).expect("serialize");
        payload.push('\n');
        stream.write_all(payload.as_bytes()).await.expect("write");

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read response");
        response
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_no_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Prepare handoff (no files accumulated).
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — no edits at all. The response is the JSON
        // envelope with a clean status and empty diagnostics output.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        let parsed: serde_json::Value =
            serde_json::from_str(response.trim()).expect("valid diagnostics envelope");
        assert_eq!(
            parsed["status"], "clean",
            "no edits is clean, got: {response}"
        );
        assert_eq!(
            parsed["output"], "",
            "expected empty diagnostics output for no edits, got: {response}",
        );

        shutdown.cancel();
    }

    #[test]
    fn out_of_roots_note_appended_to_mixed_batch() {
        use std::collections::BTreeSet;

        // A dirty covered-file receipt (the note-appension is orthogonal to how
        // clean/dirty files render in the receipt itself).
        let covered = "src/main.rs:\n\t:1:1 [error] e: boom";
        let no_roots = BTreeSet::new();

        // Nothing filtered → output is untouched.
        assert_eq!(
            with_out_of_roots_note(covered.to_string(), 0, &no_roots),
            covered
        );

        // Mixed batch: the covered-file results are preserved and the note
        // is appended so the unchecked edits are not silently hidden.
        let mixed = with_out_of_roots_note(covered.to_string(), 2, &no_roots);
        assert!(mixed.starts_with(&format!("{covered}\n")), "got: {mixed}");
        assert!(
            mixed.contains("2 edits outside tracked roots"),
            "got: {mixed}"
        );
        assert!(mixed.contains("not checked"), "got: {mixed}");

        // All-uncovered batch with no detectable roots: the plain note stands
        // alone, no stray leading newline.
        let alone = with_out_of_roots_note(String::new(), 1, &no_roots);
        assert_eq!(
            alone,
            "(1 edit outside tracked roots \u{2014} not checked; see `catenary roots -h`)",
        );
    }

    #[test]
    fn out_of_roots_note_names_detected_roots() {
        use std::collections::BTreeSet;

        // When the tracking carries the enclosing project root(s), the bare-run
        // note names them (root-aware wording, ephemeral-roots ticket 01)
        // instead of the plain count.
        let roots = BTreeSet::from([PathBuf::from("/home/dev/Projects/Lattice")]);
        let note = with_out_of_roots_note(String::new(), 1, &roots);
        assert!(
            note.contains("no language servers running for "),
            "root-aware wording: {note}"
        );
        assert!(
            note.contains("Projects/Lattice"),
            "names the detected root: {note}"
        );
        assert!(
            note.contains("see `catenary roots -h`"),
            "points at the mount command: {note}"
        );
        assert!(
            !note.contains("not checked"),
            "the plain-count wording is replaced, not appended: {note}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_out_of_roots() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Edit a file outside workspace roots — filtered, not accumulated.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": "/outside/some/file.rs",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Prepare handoff — files empty but filtered > 0.
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — should get out-of-roots message.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            response.contains("outside tracked roots"),
            "expected out-of-roots message, got: {response}",
        );
        assert!(
            response.contains("1 edit "),
            "out-of-roots message should report the filtered count, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bare_diagnostics_after_standalone_out_of_root_edit_is_not_no_edited_files() {
        // Bug 58 regression: an out-of-root edit that arrives with NO covered
        // edit alongside it (no prior `editing-start`, no in-root edit to open
        // the editing entry) used to vanish — `handle_enforce_editing` let it
        // flow free without entering editing mode, and `increment_filtered` was
        // then a no-op because no entry existed. A later bare `catenary
        // diagnostics` saw `filtered == 0` and lied with `[no edited files]`.
        // `record_filtered_edit` now creates the entry so the filtered note
        // survives.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Edit a file outside every root as the FIRST action — no editing-start,
        // no covered edit to open the entry.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": "/outside/some/file.rs",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Prepare + run bare diagnostics.
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            response.contains("outside tracked roots"),
            "the standalone out-of-root edit must surface the filtered note, got: {response}",
        );
        assert!(
            response.contains("1 edit "),
            "the filtered count must be 1, got: {response}",
        );
        // The receipt output must NOT be the bare `[no edited files]` lie.
        let parsed: serde_json::Value =
            serde_json::from_str(response.trim()).expect("valid diagnostics envelope");
        let output = parsed["output"].as_str().unwrap_or_default();
        assert!(
            !output.trim().is_empty(),
            "output must carry the filtered note, not empty (→ CLI [no edited files]): {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_expired() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Call done-editing/run without preparing a handoff.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            response.contains("handoff expired"),
            "expected handoff expired message, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_with_accumulated_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Accumulate a file via pre-tool hook (file tracking).
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": "/tmp/nonexistent_file.rs",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // Prepare handoff — snapshots the accumulated file.
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Execute done_editing/run — diagnostics pipeline runs on
        // the files. Since there's no real LSP server, the output
        // depends on whether the file exists and has LSP coverage.
        // The key test: the handoff consumed the files successfully.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &req).await;
        // With no LSP servers the file is uncovered (rendered as
        // "[no LSP coverage]", not "[clean]" — a `[clean]` line is for a
        // covered file a feeder verified, not an uncovered one). The response
        // should not be the expired message.
        assert!(
            !response.contains("handoff expired"),
            "handoff should not be expired, got: {response}",
        );

        shutdown.cancel();
    }

    /// Regression guard for bug 37 (under the misc-141 batch model): two agents
    /// share one Catenary session (a subagent and the main agent, distinguished
    /// only by `agent_id`). A `catenary diagnostics` for one `agent_id` must flip
    /// ONLY that agent's batch — the sibling agent's batch must stay armed so its
    /// own later `catenary diagnostics` still reports its edits. Two
    /// `(session_id, agent_id)` pairs never share a batch.
    ///
    /// The flip is deferred to *delivery* (the consume step's socket write), not
    /// the prepare (`pre-tool/editing-stop`): the prepare only snapshots. So the
    /// assertion runs after the consume.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_flips_only_requesting_agent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Both agents enter editing mode under the same session. Sending the
        // editing-start hooks creates the session and its EditingManager.
        for agent in ["sub-a", ""] {
            let req = serde_json::json!({
                "method": "pre-tool/editing-start",
                "agent_id": agent,
                "session_id": "sess-1"
            });
            let _ = hook_roundtrip(&ipc_path, &req).await;
        }

        // Stage distinct accumulated files per agent. This harness configures
        // no covered root, so accumulate via the session's editing manager
        // directly (mirrors session_state_editing_per_session).
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router.session.editing.record_covered_edit(
                Some("sess-1"),
                "sub-a",
                std::path::PathBuf::from("/src/a.rs"),
            );
            router.session.editing.record_covered_edit(
                Some("sess-1"),
                "",
                std::path::PathBuf::from("/src/b.rs"),
            );
        }

        // Prepare the handoff for the subagent only — this snapshots the
        // subagent's batch.
        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "sub-a",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Consume the handoff — delivery flips the subagent's batch. The identity
        // rides the staged payload, so the bare consume request carries none.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let _ = hook_roundtrip_full(&ipc_path, &req).await;

        // The subagent's batch is delivered (gate disarmed); the main agent's
        // batch stays armed — the two never shared state.
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert!(
                !router
                    .session
                    .editing
                    .has_undelivered(Some("sess-1"), "sub-a"),
                "subagent's batch is delivered after its consume — gate disarmed"
            );
            assert!(
                router.session.editing.has_undelivered(Some("sess-1"), ""),
                "the main agent's batch stays armed (bug 37 / misc 141)"
            );
            assert_eq!(
                router.session.editing.files(Some("sess-1"), ""),
                vec![std::path::PathBuf::from("/src/b.rs")],
                "main agent's batch is untouched by the subagent's delivery"
            );
        }

        shutdown.cancel();
    }

    /// Core regression for bug 32 (under the misc-141 batch model): a failed
    /// `catenary diagnostics <path>` attempt fires `pre-tool/editing-stop`
    /// (prepare) but never consumes the slot (clap rejects it before the IPC, or
    /// the host kills the subprocess). The prepare only SNAPSHOTS — it never
    /// mutates the batch — so the batch survives the abandoned attempt armed, and
    /// a subsequent valid `catenary diagnostics` still reports the edited files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_diagnostics_attempt_preserves_edited_set() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode and accumulate a tracked file.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router.session.editing.record_covered_edit(
                Some("sess-1"),
                "",
                std::path::PathBuf::from("/src/edited.rs"),
            );
        }

        // The FAILED attempt: the PreToolUse hook fires prepare, but the
        // malformed `catenary diagnostics <path>` exits before connecting, so
        // the slot is never consumed. The set must survive.
        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &prepare).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert!(
                router.session.editing.has_undelivered(Some("sess-1"), ""),
                "snapshot prepare must NOT flip — the batch stays armed after the failed attempt"
            );
            assert_eq!(
                router.session.editing.files(Some("sess-1"), ""),
                vec![std::path::PathBuf::from("/src/edited.rs")],
                "the batch survives the failed attempt intact"
            );
        }

        // The corrective VALID attempt: prepare again (re-snapshots the still
        // present batch), then consume. The batch is still reported, then flipped
        // to delivered on the successful response write.
        let line = hook_roundtrip(&ipc_path, &prepare).await;
        assert!(
            line.contains("ok"),
            "second prepare should succeed, got: {line}"
        );

        let consume = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &consume).await;
        assert!(
            !response.contains("handoff expired"),
            "valid consume must find the staged files, got: {response}",
        );

        // After delivery the batch is retained but delivered — the gate disarms
        // (misc 141), while the batch stays available for a repeat bare re-run.
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert!(
                !router.session.editing.has_undelivered(Some("sess-1"), ""),
                "delivery disarms the gate (flags flip, batch retained)"
            );
            assert_eq!(
                router.session.editing.files(Some("sess-1"), ""),
                vec![std::path::PathBuf::from("/src/edited.rs")],
                "the batch is retained for a repeat bare re-diagnosis"
            );
        }

        shutdown.cancel();
    }

    /// ws37 ticket 02 (under the misc-141 batch model): a scoped
    /// `catenary diagnostics <path>` pays only the named file's debt. The consume
    /// carries an explicit `files` param; the daemon flips ONLY those flags on
    /// delivery and, because undelivered debt remains, keeps the editing guardrail
    /// armed. The batch retains every file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_diagnostics_pays_partial_debt_keeps_gate_armed() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        let root = dir.path().to_path_buf();
        let file_a = root.join("a.rs");
        let file_b = root.join("b.rs");
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_a.clone());
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_b.clone());
            // Arm the guardrail on the root the way a covered edit would.
            ctx.editing_guardrail
                .try_acquire(&root, "sess-1")
                .expect("arm guardrail");
        }

        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let line = hook_roundtrip(&ipc_path, &prepare).await;
        assert!(line.contains("ok"), "prepare should succeed, got: {line}");

        // Scoped consume: pay only file_a's debt.
        let consume = serde_json::json!({
            "method": "tool/editing-stop",
            "files": [file_a.to_string_lossy()],
        });
        let response = hook_roundtrip_full(&ipc_path, &consume).await;
        assert!(
            !response.contains("handoff expired"),
            "scoped consume must find the staged handoff, got: {response}",
        );

        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert_eq!(
                router.session.editing.undelivered_files(Some("sess-1"), ""),
                vec![file_b.clone()],
                "a scoped pull flips only the named file; the rest stays undelivered",
            );
            assert_eq!(
                router.session.editing.files(Some("sess-1"), "").len(),
                2,
                "the batch retains both files",
            );
            // Debt remains → the guardrail stays armed: another session can't
            // claim the root.
            assert!(
                ctx.editing_guardrail.try_acquire(&root, "other").is_err(),
                "a partial pull leaves debt, so the gate stays armed",
            );
        }

        shutdown.cancel();
    }

    /// ws37 ticket 02 (under the misc-141 batch model): a bare
    /// `catenary diagnostics` pays the whole debt. The guardrail release is
    /// conditional on delivery — the bare form flips every flag, so nothing stays
    /// undelivered and the gate releases. The batch itself is retained.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bare_diagnostics_pays_all_debt_releases_gate() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        let root = dir.path().to_path_buf();
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", root.join("a.rs"));
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", root.join("b.rs"));
            ctx.editing_guardrail
                .try_acquire(&root, "sess-1")
                .expect("arm guardrail");
        }

        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &prepare).await;

        // Bare consume (no `files`): re-diagnoses the whole batch and flips
        // every flag on delivery.
        let consume = serde_json::json!({"method": "tool/editing-stop"});
        let _ = hook_roundtrip_full(&ipc_path, &consume).await;

        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert!(
                !router.session.editing.has_undelivered(Some("sess-1"), ""),
                "a bare pull delivers the whole batch — nothing stays undelivered",
            );
            assert_eq!(
                router.session.editing.files(Some("sess-1"), "").len(),
                2,
                "the batch is retained for a repeat bare re-diagnosis",
            );
            // No undelivered debt → the guardrail released: another session can
            // claim the root.
            assert!(
                ctx.editing_guardrail.try_acquire(&root, "other").is_ok(),
                "the bare form delivers everything, so the gate releases on delivery",
            );
        }

        shutdown.cancel();
    }

    /// misc 141 (idiom fix): a bare `catenary diagnostics` run over a completed
    /// batch re-diagnoses the SAME batch instead of `[no edited files]`. The
    /// batch is durable daemon state; delivery flips its flags but retains it, so
    /// a repeat bare run (no intervening edit) computes fresh over the same scope.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeat_bare_re_diagnoses_the_same_batch() {
        // Drive a bare run (prepare + consume) and return its parsed envelope.
        async fn bare_run(ipc_path: &Path) -> serde_json::Value {
            let prepare = serde_json::json!({
                "method": "pre-tool/editing-stop",
                "agent_id": "",
                "session_id": "sess-1"
            });
            let _ = hook_roundtrip(ipc_path, &prepare).await;
            let response = hook_roundtrip_full(
                ipc_path,
                &serde_json::json!({"method": "tool/editing-stop"}),
            )
            .await;
            serde_json::from_str(response.trim()).expect("valid diagnostics envelope")
        }

        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let start = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &start).await;

        let root = dir.path().to_path_buf();
        let file_a = root.join("a.rs");
        let file_b = root.join("b.rs");
        std::fs::write(&file_a, "fn a() {}\n").expect("write a.rs");
        std::fs::write(&file_b, "fn b() {}\n").expect("write b.rs");
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_a.clone());
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_b.clone());
        }

        // Run 1: covers the whole batch, flips every flag on delivery.
        let first = bare_run(&ipc_path).await;
        assert_eq!(
            first["covered"].as_u64(),
            Some(2),
            "run 1 covers the whole batch, got: {first}",
        );
        assert!(
            !first["output"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "run 1 renders a receipt, got: {first}",
        );

        // Run 2, with NO intervening edit: the batch is retained, so a repeat
        // bare run re-diagnoses the same scope instead of `[no edited files]`.
        let second = bare_run(&ipc_path).await;
        assert_eq!(
            second["covered"].as_u64(),
            Some(2),
            "repeat bare re-diagnoses the same batch (not `[no edited files]`), got: {second}",
        );
        assert!(
            !second["output"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "run 2 renders a fresh receipt over the same batch, got: {second}",
        );

        shutdown.cancel();
    }

    /// misc 141 / bug 60: an undelivered bare run leaves the batch's flags false
    /// and the gate armed. Modeled as computed-but-undelivered — the client
    /// half-closes its write side, so the daemon's disconnect-cancel select
    /// (bug 24) fires and no response reaches it, deterministically avoiding the
    /// flip. The batch survives, and the next bare run re-serves it in full.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn undelivered_run_leaves_flags_false_and_gate_armed() {
        // Send a request and half-close the write side without reading — the
        // daemon reads EOF on its cancel probe and returns before writing any
        // response (bug 24), so delivery never happens.
        async fn send_and_halfclose(ipc_path: &Path, request: &serde_json::Value) {
            use tokio::io::AsyncWriteExt;
            let stream = tokio::net::UnixStream::connect(ipc_path)
                .await
                .expect("connect to IPC socket");
            let (_read, mut write) = stream.into_split();
            let mut payload = serde_json::to_string(request).expect("serialize");
            payload.push('\n');
            write.write_all(payload.as_bytes()).await.expect("write");
            write.shutdown().await.expect("shutdown write half");
        }

        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let start = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &start).await;

        let root = dir.path().to_path_buf();
        let file_a = root.join("a.rs");
        std::fs::write(&file_a, "fn a() {}\n").expect("write a.rs");
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_a.clone());
        }

        // Prepare, then run a consume whose client half-closes: the disconnect-
        // cancel path fires, so the batch's flag never flips.
        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &prepare).await;
        send_and_halfclose(
            &ipc_path,
            &serde_json::json!({"method": "tool/editing-stop"}),
        )
        .await;

        // The flag never flipped: the gate stays armed and the batch is intact.
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert!(
                router.session.editing.has_undelivered(Some("sess-1"), ""),
                "an undelivered run must leave the gate armed (flags false)"
            );
            assert_eq!(
                router.session.editing.files(Some("sess-1"), ""),
                vec![file_a.clone()],
                "the batch survives the undelivered run intact"
            );
        }

        // Recovery: the next bare run re-serves the whole batch (the bug-60 shape).
        let _ = hook_roundtrip(&ipc_path, &prepare).await;
        let response = hook_roundtrip_full(
            &ipc_path,
            &serde_json::json!({"method": "tool/editing-stop"}),
        )
        .await;
        let parsed: serde_json::Value =
            serde_json::from_str(response.trim()).expect("valid diagnostics envelope");
        assert_eq!(
            parsed["covered"].as_u64(),
            Some(1),
            "the next bare run covers the batch (bug-60 recovery), got: {response}",
        );

        shutdown.cancel();
    }

    /// misc 141: two `(session_id, agent_id)` pairs never share a batch. Each
    /// session accumulates and delivers its own file; one session's delivery
    /// leaves the other's batch untouched, and neither batch names the other's
    /// file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_sessions_never_share_a_batch() {
        // Drive one session through a full bare run over a single distinctive
        // file (declared first so it precedes the test's statements).
        async fn run_session(
            ipc_path: &Path,
            manager: &SessionManager,
            session_id: &str,
            file: &Path,
        ) {
            let start = serde_json::json!({
                "method": "pre-tool/editing-start",
                "agent_id": "",
                "session_id": session_id,
            });
            let _ = hook_roundtrip(ipc_path, &start).await;
            {
                let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
                let sessions = ctx.sessions.lock().expect("lock");
                let router = Arc::clone(&sessions.get(session_id).expect("session").router);
                drop(sessions);
                router.session.editing.record_covered_edit(
                    Some(session_id),
                    "",
                    file.to_path_buf(),
                );
            }
            let prepare = serde_json::json!({
                "method": "pre-tool/editing-stop",
                "agent_id": "",
                "session_id": session_id,
            });
            let _ = hook_roundtrip(ipc_path, &prepare).await;
            let _ = hook_roundtrip_full(
                ipc_path,
                &serde_json::json!({"method": "tool/editing-stop"}),
            )
            .await;
        }

        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let root = dir.path().to_path_buf();
        let file_one = root.join("sess_one_file.rs");
        let file_two = root.join("sess_two_file.rs");
        std::fs::write(&file_one, "fn one() {}\n").expect("write file one");
        std::fs::write(&file_two, "fn two() {}\n").expect("write file two");

        run_session(&ipc_path, &manager, "sess-1", &file_one).await;
        run_session(&ipc_path, &manager, "sess-2", &file_two).await;

        let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
        let sessions = ctx.sessions.lock().expect("lock");
        let router_one = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
        let router_two = Arc::clone(&sessions.get("sess-2").expect("sess-2").router);
        drop(sessions);

        // Each session's batch holds only its own file, and each was delivered by
        // its own run — the two batches never crossed.
        assert_eq!(
            router_one.session.editing.files(Some("sess-1"), ""),
            vec![file_one.clone()],
            "sess-1's batch holds only its own file",
        );
        assert_eq!(
            router_two.session.editing.files(Some("sess-2"), ""),
            vec![file_two.clone()],
            "sess-2's batch holds only its own file",
        );
        assert!(
            !router_one
                .session
                .editing
                .has_undelivered(Some("sess-1"), ""),
            "sess-1's batch was delivered by its own run",
        );
        assert!(
            !router_two
                .session
                .editing
                .has_undelivered(Some("sess-2"), ""),
            "sess-2's batch was delivered by its own run",
        );

        shutdown.cancel();
    }

    /// misc 141: a fresh session (no edits) still reports `[no edited files]` — a
    /// bare run over an empty batch is covered 0, exactly as today.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_session_bare_reports_no_edited_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // No editing-start, no edits — a fresh session's first bare run.
        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &prepare).await;
        let response = hook_roundtrip_full(
            &ipc_path,
            &serde_json::json!({"method": "tool/editing-stop"}),
        )
        .await;
        let parsed: serde_json::Value =
            serde_json::from_str(response.trim()).expect("valid diagnostics envelope");
        assert_eq!(
            parsed["covered"].as_u64(),
            Some(0),
            "a fresh session's bare run is covered 0 (`[no edited files]`), got: {response}",
        );

        shutdown.cancel();
    }

    /// ws37 ticket 02 (under the misc-141 batch model): re-editing a paid file
    /// re-arms the gate. After a scoped pull delivers the only file and releases
    /// the guardrail, editing that file again discards the completed batch, starts
    /// a fresh one with the file undelivered, and re-locks the root.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_editing_a_paid_file_rearms_the_gate() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        let root = dir.path().to_path_buf();
        let file_a = root.join("a.rs");
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_a.clone());
            ctx.editing_guardrail
                .try_acquire(&root, "sess-1")
                .expect("arm guardrail");
        }

        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &prepare).await;

        // Pay file_a's debt in full (scoped delivery of the only file).
        let consume = serde_json::json!({
            "method": "tool/editing-stop",
            "files": [file_a.to_string_lossy()],
        });
        let _ = hook_roundtrip_full(&ipc_path, &consume).await;

        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert!(
                !router.session.editing.has_undelivered(Some("sess-1"), ""),
                "delivering the only file disarms the gate (batch now complete)",
            );
            // Released: probe then free it so the re-edit below can re-lock.
            assert!(
                ctx.editing_guardrail.try_acquire(&root, "other").is_ok(),
                "delivering the only file releases the guardrail",
            );
            ctx.editing_guardrail.release(&root, "other");

            // Re-edit re-arms: the edit hook re-affirms editing mode
            // (`start_editing` is a no-op — the batch entry is retained), then
            // records the covered edit. The batch was complete, so this discards
            // it and starts a fresh, undelivered batch with the file, re-locking
            // the root.
            let _ = router.session.editing.start_editing(Some("sess-1"), "");
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_a.clone());
            ctx.editing_guardrail
                .try_acquire(&root, "sess-1")
                .expect("re-arm guardrail on re-edit");
            assert_eq!(
                router.session.editing.files(Some("sess-1"), ""),
                vec![file_a.clone()],
                "the re-edited file is the sole member of the fresh batch",
            );
            assert!(
                router.session.editing.has_undelivered(Some("sess-1"), ""),
                "re-editing a paid file re-arms the gate (undelivered again)",
            );
            assert!(
                ctx.editing_guardrail.try_acquire(&root, "other").is_err(),
                "re-editing a paid file re-locks the root",
            );
        }

        shutdown.cancel();
    }

    /// ws37 ticket 02 (under the misc-141 batch model): a scoped pull of an
    /// UNEDITED path reports diagnostics but the flag-flip is a no-op — the batch
    /// is unchanged and the guardrail stays armed. One pipeline serves "lint this"
    /// and "verify my edit."
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_diagnostics_unedited_path_is_noop() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        let root = dir.path().to_path_buf();
        let edited = root.join("edited.rs");
        let unedited = root.join("unedited.rs");
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", edited.clone());
            ctx.editing_guardrail
                .try_acquire(&root, "sess-1")
                .expect("arm guardrail");
        }

        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &prepare).await;

        // Scoped pull of a file NOT in the debt set.
        let consume = serde_json::json!({
            "method": "tool/editing-stop",
            "files": [unedited.to_string_lossy()],
        });
        let response = hook_roundtrip_full(&ipc_path, &consume).await;
        assert!(
            !response.contains("handoff expired"),
            "an unedited scoped pull still rides the handoff (attribution fires), got: {response}",
        );

        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert_eq!(
                router.session.editing.files(Some("sess-1"), ""),
                vec![edited.clone()],
                "querying an unedited file flips nothing — the batch is unchanged",
            );
            assert!(
                router.session.editing.has_undelivered(Some("sess-1"), ""),
                "the edited file stays undelivered — the gate is unmoved",
            );
            assert!(
                ctx.editing_guardrail.try_acquire(&root, "other").is_err(),
                "a no-op flip leaves the gate armed",
            );
        }

        shutdown.cancel();
    }

    /// ws37 ticket 02 (under the misc-141 batch model): a scoped consume whose
    /// handoff was never prepared (a faulted/denied round-trip) is an expired
    /// slot — it flips nothing, leaving the batch intact and armed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_diagnostics_without_handoff_leaves_debt_intact() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        let root = dir.path().to_path_buf();
        let file_a = root.join("a.rs");
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router
                .session
                .editing
                .record_covered_edit(Some("sess-1"), "", file_a.clone());
        }

        // Consume WITHOUT a prepared handoff: the slot is empty (expired), so no
        // flip keys are captured and the batch is left untouched.
        let consume = serde_json::json!({
            "method": "tool/editing-stop",
            "files": [file_a.to_string_lossy()],
        });
        let response = hook_roundtrip_full(&ipc_path, &consume).await;
        assert!(
            response.contains("handoff expired"),
            "a scoped consume with no staged handoff is an expired slot, got: {response}",
        );

        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            assert_eq!(
                router.session.editing.files(Some("sess-1"), ""),
                vec![file_a.clone()],
                "a faulted scoped call flips nothing — the batch is intact",
            );
            assert!(
                router.session.editing.has_undelivered(Some("sess-1"), ""),
                "the batch stays armed after a faulted scoped call",
            );
        }

        shutdown.cancel();
    }

    /// Bug 32 secondary: the consume envelope carries a `covered` file count so
    /// the CLI can synthesize a `[no edited files]` sentinel for the 0-file
    /// case. A consume over an empty handoff reports `covered: 0`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_reports_covered_zero_when_empty() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode but accumulate NO files, then prepare + consume.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;
        let prepare = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &prepare).await;

        let consume = serde_json::json!({"method": "tool/editing-stop"});
        let response = hook_roundtrip_full(&ipc_path, &consume).await;
        let parsed: serde_json::Value =
            serde_json::from_str(response.trim()).expect("consume response is JSON");
        assert_eq!(
            parsed.get("covered").and_then(serde_json::Value::as_u64),
            Some(0),
            "an empty handoff reports covered: 0 for the CLI sentinel, got: {response}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_editing_handoff_double_consume() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Enter editing mode and prepare handoff.
        let req = serde_json::json!({
            "method": "pre-tool/editing-start",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        let req = serde_json::json!({
            "method": "pre-tool/editing-stop",
            "agent_id": "",
            "session_id": "sess-1"
        });
        let _ = hook_roundtrip(&ipc_path, &req).await;

        // First consume should succeed.
        let req = serde_json::json!({"method": "tool/editing-stop"});
        let response1 = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            !response1.contains("handoff expired"),
            "first consume should succeed, got: {response1}",
        );

        // Second consume should see expired slot.
        let response2 = hook_roundtrip_full(&ipc_path, &req).await;
        assert!(
            response2.contains("handoff expired"),
            "second consume should see expired slot, got: {response2}",
        );

        shutdown.cancel();
    }

    // ── Keyed handoff structure tests (ADR 014) ───────────────────────

    /// The same key serializes in order: a second `diagnostics` acquire blocks
    /// until the first permit is released (no overwrite, no double-consume).
    #[tokio::test]
    async fn keyed_handoff_same_key_serializes() {
        let handoff = KeyedHandoff::new();

        let first = handoff
            .acquire(HandoffKey::Diagnostics)
            .await
            .expect("first acquire");

        // A second same-key acquire blocks while the first permit is held —
        // the timeout elapses rather than completing.
        let blocked = tokio::time::timeout(
            Duration::from_millis(200),
            handoff.acquire(HandoffKey::Diagnostics),
        )
        .await;
        assert!(
            blocked.is_err(),
            "second diagnostics acquire must block while the first is held",
        );

        // Releasing the first lets the second proceed.
        drop(first);
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            handoff.acquire(HandoffKey::Diagnostics),
        )
        .await;
        assert!(
            second.is_ok(),
            "second diagnostics acquire must proceed once the first is released",
        );
    }

    /// Stage → consume round-trips the payload, frees the permit, and a second
    /// consume sees the empty slot.
    #[tokio::test]
    async fn keyed_handoff_stage_consume_roundtrip() {
        let handoff = KeyedHandoff::new();

        let permit = handoff
            .acquire(HandoffKey::Diagnostics)
            .await
            .expect("acquire");
        handoff.stage(
            HandoffKey::Diagnostics,
            HandoffContext {
                parent_id: "scope-1".to_string(),
                payload: HandoffPayload::Diagnostics {
                    files: vec![PathBuf::from("/tmp/a.rs")],
                    filtered: 2,
                    filtered_roots: std::collections::BTreeSet::from([PathBuf::from(
                        "/home/dev/Lattice",
                    )]),
                    session_id: "sess-1".to_string(),
                    editing_session: Some("sess-1".to_string()),
                    agent_id: String::new(),
                },
                permit,
            },
        );

        let consumed = handoff
            .consume(HandoffKey::Diagnostics)
            .expect("consume staged context");
        assert_eq!(consumed.parent_id, "scope-1");
        let HandoffPayload::Diagnostics {
            files,
            filtered,
            filtered_roots,
            session_id,
            editing_session,
            agent_id,
        } = &consumed.payload;
        assert_eq!(files, &vec![PathBuf::from("/tmp/a.rs")]);
        assert_eq!(*filtered, 2);
        assert_eq!(
            filtered_roots,
            &std::collections::BTreeSet::from([PathBuf::from("/home/dev/Lattice")])
        );
        assert_eq!(session_id, "sess-1");
        assert_eq!(editing_session.as_deref(), Some("sess-1"));
        assert_eq!(agent_id, "");

        // Slot is now empty — a second consume yields None.
        assert!(
            handoff.consume(HandoffKey::Diagnostics).is_none(),
            "double consume must yield None",
        );

        // Dropping the consumed context frees the permit for the next stage.
        drop(consumed);
        let reacquire = tokio::time::timeout(
            Duration::from_secs(1),
            handoff.acquire(HandoffKey::Diagnostics),
        )
        .await;
        assert!(reacquire.is_ok(), "permit must be free after consume");
    }

    /// A never-connecting stage is cleared by its per-key timeout, releasing the
    /// permit for the next same-key stage.
    ///
    /// Injects a short self-heal timeout via [`KeyedHandoff::with_timeout`] so
    /// the clear-on-timeout path runs fast, independent of the production
    /// [`HANDOFF_TIMEOUT`].
    #[tokio::test]
    async fn keyed_handoff_timeout_clears_stage() {
        let handoff = KeyedHandoff::with_timeout(Duration::from_millis(50));

        let permit = handoff
            .acquire(HandoffKey::Diagnostics)
            .await
            .expect("acquire");
        handoff.stage(
            HandoffKey::Diagnostics,
            HandoffContext {
                parent_id: "x".to_string(),
                payload: HandoffPayload::Diagnostics {
                    files: Vec::new(),
                    filtered: 0,
                    filtered_roots: std::collections::BTreeSet::new(),
                    session_id: "x".to_string(),
                    editing_session: Some("x".to_string()),
                    agent_id: String::new(),
                },
                permit,
            },
        );

        // Wait past the (short, test-only) per-key timeout; the spawned task
        // clears the slot.
        tokio::time::sleep(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;

        assert!(
            handoff.consume(HandoffKey::Diagnostics).is_none(),
            "diagnostics stage must be cleared after its timeout",
        );

        // The permit was released on timeout — a fresh acquire proceeds.
        let _diag = handoff
            .acquire(HandoffKey::Diagnostics)
            .await
            .expect("permit released on timeout");
    }

    // ── Version handshake tests ──────────────────────────────────────

    /// Helper: build an initialize request JSON line.
    fn init_request_line() -> String {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        });
        format!("{}\n", serde_json::to_string(&request).expect("serialize"))
    }

    /// Spawn a fake daemon thread that reads an initialize request and
    /// responds with the given version in `serverInfo`.
    fn fake_daemon(
        stream: std::os::unix::net::UnixStream,
        version: &str,
    ) -> std::thread::JoinHandle<()> {
        let version = version.to_string();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut reader = std::io::BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read init request");

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {
                        "name": "catenary",
                        "version": version,
                    }
                }
            });
            let mut w: &std::os::unix::net::UnixStream = &stream;
            writeln!(w, "{}", serde_json::to_string(&response).expect("ser"))
                .expect("write response");
        })
    }

    #[test]
    fn matching_version_connects() {
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        let handle = fake_daemon(server_sock, env!("CATENARY_VERSION"));

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        version_handshake(&mut stdin, &client_sock, &mut stdout)
            .expect("handshake should succeed with matching version");
        handle.join().expect("daemon thread");

        assert!(!stdout.is_empty(), "response should be forwarded to stdout");
        let response: serde_json::Value =
            serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim())
                .expect("parse response");
        assert_eq!(response["result"]["serverInfo"]["name"], "catenary");
        assert_eq!(
            response["result"]["serverInfo"]["version"],
            env!("CATENARY_VERSION"),
        );
    }

    #[test]
    fn mismatched_version_rejected() {
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        let handle = fake_daemon(server_sock, "0.0.0-fake");

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        let result = version_handshake(&mut stdin, &client_sock, &mut stdout);
        handle.join().expect("daemon thread");

        assert!(result.is_err(), "handshake should fail on version mismatch");
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("version mismatch"),
            "error should mention mismatch: {err}",
        );
        assert!(
            err.contains("0.0.0-fake"),
            "error should contain daemon version: {err}",
        );
        assert!(stdout.is_empty(), "should not forward response on mismatch");
    }

    #[test]
    fn missing_version_rejected() {
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        // Daemon responds without a version field in serverInfo.
        let handle = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut reader = std::io::BufReader::new(&server_sock);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read init request");

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": { "name": "not-catenary" }
                }
            });
            let mut w: &std::os::unix::net::UnixStream = &server_sock;
            writeln!(w, "{}", serde_json::to_string(&response).expect("ser"))
                .expect("write response");
        });

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        let result = version_handshake(&mut stdin, &client_sock, &mut stdout);
        handle.join().expect("daemon thread");

        assert!(
            result.is_err(),
            "handshake should fail when version is missing"
        );
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("did not report a version"),
            "error should explain missing version: {err}",
        );
    }

    // ── Root refcounting tests ────────────────────────────────────────

    #[test]
    fn single_session_adds_roots() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo"), PathBuf::from("/bar")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 2);
        assert!(global.contains(&PathBuf::from("/foo")));
        assert!(global.contains(&PathBuf::from("/bar")));
    }

    #[test]
    fn two_sessions_shared_root() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.set_roots("mcp:20", vec![PathBuf::from("/foo"), PathBuf::from("/bar")]);

        assert_eq!(tracker.refcount(Path::new("/foo")), 2);
        assert_eq!(tracker.refcount(Path::new("/bar")), 1);

        // Remove first session — /foo should survive (refcount 1).
        tracker.remove_contributor("mcp:10");

        let global = tracker.global_roots();
        assert!(
            global.contains(&PathBuf::from("/foo")),
            "/foo should survive"
        );
        assert!(
            global.contains(&PathBuf::from("/bar")),
            "/bar should survive"
        );
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);
    }

    #[test]
    fn last_session_removes_root() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.set_roots("mcp:20", vec![PathBuf::from("/foo")]);

        // Remove first — still has refcount 1.
        tracker.remove_contributor("mcp:10");
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);

        // Remove second — refcount 0, gone from global set.
        tracker.remove_contributor("mcp:20");
        assert_eq!(tracker.refcount(Path::new("/foo")), 0);
        assert!(tracker.global_roots().is_empty());
    }

    #[test]
    fn add_dir_increments_refcount() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        // Transcript scan adds a root for the same session.
        tracker.add_roots("transcript:sess-a", &[PathBuf::from("/bar")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 2);
        assert!(global.contains(&PathBuf::from("/foo")));
        assert!(global.contains(&PathBuf::from("/bar")));
    }

    #[test]
    fn duplicate_root_same_session_no_double_count() {
        let tracker = RootTracker::new();

        // Same contributor sets the same root via set_roots (idempotent).
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);

        // add_roots also deduplicates within the same contributor.
        tracker.add_roots("mcp:10", &[PathBuf::from("/foo")]);
        assert_eq!(tracker.refcount(Path::new("/foo")), 1);
    }

    #[test]
    fn set_roots_replaces_previous() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo"), PathBuf::from("/bar")]);
        tracker.set_roots("mcp:10", vec![PathBuf::from("/baz")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 1);
        assert!(global.contains(&PathBuf::from("/baz")));
        assert!(!global.contains(&PathBuf::from("/foo")));
    }

    #[test]
    fn remove_nonexistent_contributor_is_noop() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.remove_contributor("mcp:99");

        assert_eq!(tracker.global_roots().len(), 1);
    }

    #[test]
    fn remove_contributors_with_prefix_removes_all_matching_keys() {
        let tracker = RootTracker::new();
        tracker.set_roots("worktree:sess-a:/wt1", vec![PathBuf::from("/wt1")]);
        tracker.set_roots("worktree:sess-a:/wt2", vec![PathBuf::from("/wt2")]);

        let removed = tracker.remove_contributors_with_prefix("worktree:sess-a:");

        assert_eq!(removed, 2, "both worktree keys for the session removed");
        assert!(
            tracker.global_roots().is_empty(),
            "all of the session's worktree roots gone"
        );
    }

    #[test]
    fn remove_contributors_with_prefix_leaves_non_matching_keys() {
        let tracker = RootTracker::new();
        tracker.set_roots("worktree:sess-a:/wt1", vec![PathBuf::from("/wt1")]);
        tracker.set_roots("worktree:sess-b:/wt2", vec![PathBuf::from("/wt2")]);
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        let removed = tracker.remove_contributors_with_prefix("worktree:sess-a:");

        assert_eq!(removed, 1, "only the matching session's key removed");
        let global = tracker.global_roots();
        assert!(
            !global.contains(&PathBuf::from("/wt1")),
            "sess-a worktree dropped"
        );
        assert!(
            global.contains(&PathBuf::from("/wt2")),
            "sess-b worktree (different prefix) untouched"
        );
        assert!(
            global.contains(&PathBuf::from("/foo")),
            "mcp contributor (different prefix) untouched"
        );
    }

    #[test]
    fn remove_contributors_with_prefix_no_match_is_noop() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        let removed = tracker.remove_contributors_with_prefix("worktree:sess-z:");

        assert_eq!(removed, 0, "nothing matched the prefix");
        assert_eq!(
            tracker.global_roots().len(),
            1,
            "non-matching contributor survives"
        );
    }

    #[test]
    fn remove_contributors_with_prefix_empty_prefix_removes_all() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);
        tracker.set_roots("worktree:sess-a:/wt1", vec![PathBuf::from("/wt1")]);

        // An empty prefix is a prefix of every key; the count must reflect it.
        let removed = tracker.remove_contributors_with_prefix("");

        assert_eq!(removed, 2, "every key starts_with the empty prefix");
        assert!(tracker.global_roots().is_empty());
    }

    #[test]
    fn contributors_with_prefix_returns_matching_keys_with_roots() {
        let tracker = RootTracker::new();
        tracker.set_roots("worktree:sess-a:/wt1", vec![PathBuf::from("/wt1")]);
        tracker.set_roots("worktree:sess-b:/wt2", vec![PathBuf::from("/wt2")]);
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        let mut got = tracker.contributors_with_prefix("worktree:");
        got.sort_by(|(a, _), (b, _)| a.cmp(b));

        assert_eq!(got.len(), 2, "only the worktree:* contributors enumerated");
        assert_eq!(got[0].0, "worktree:sess-a:/wt1");
        assert_eq!(got[0].1, vec![PathBuf::from("/wt1")]);
        assert_eq!(got[1].0, "worktree:sess-b:/wt2");
        assert_eq!(got[1].1, vec![PathBuf::from("/wt2")]);
    }

    #[test]
    fn contributors_with_prefix_no_match_is_empty() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        assert!(
            tracker.contributors_with_prefix("worktree:").is_empty(),
            "no worktree:* contributor → empty enumeration",
        );
    }

    // ── Daemon root-GC: reap_missing_worktree_roots (workstream 30) ───────
    //
    // The crash-safe leak backstop's single pass: reap every `worktree:*`
    // contributor whose dir is gone on disk, keep those whose dir survives, and
    // never inspect a non-worktree contributor. Real tempdirs make the
    // path-existence signal authoritative.

    #[test]
    fn reap_missing_worktree_roots_reaps_only_gone_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");

        // One worktree dir that exists (kept).
        let existing = base.join("existing");
        std::fs::create_dir(&existing).expect("mkdir existing");

        // One worktree dir that we create then remove (reaped).
        let gone = base.join("gone");
        std::fs::create_dir(&gone).expect("mkdir gone");

        let tracker = RootTracker::new();
        tracker.set_roots(
            &format!("worktree:sess:{}", existing.display()),
            vec![existing.clone()],
        );
        tracker.set_roots(
            &format!("worktree:sess:{}", gone.display()),
            vec![gone.clone()],
        );
        // A non-worktree contributor — never inspected, always kept.
        tracker.set_roots("mcp:1", vec![base.join("project")]);

        // Now the dir vanishes (a missed WorktreeRemove after the dir is gone).
        std::fs::remove_dir_all(&gone).expect("remove gone");

        let removed = reap_missing_worktree_roots(&tracker);

        assert_eq!(
            removed,
            vec![format!("worktree:sess:{}", gone.display())],
            "exactly the worktree whose dir is gone is reaped",
        );
        let global = tracker.global_roots();
        assert!(
            global.contains(&existing),
            "the existing worktree dir survives",
        );
        assert!(
            !global.contains(&gone),
            "the gone worktree dir is reclaimed",
        );
        assert!(
            global.contains(&base.join("project")),
            "the non-worktree (mcp:*) contributor is never inspected",
        );
    }

    #[test]
    fn reap_missing_worktree_roots_noop_when_all_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let wt = base.join("wt");
        std::fs::create_dir(&wt).expect("mkdir wt");

        let tracker = RootTracker::new();
        let key = format!("worktree:sess:{}", wt.display());
        tracker.set_roots(&key, vec![wt]);
        tracker.set_roots("mcp:1", vec![base.join("project")]);

        assert!(
            reap_missing_worktree_roots(&tracker).is_empty(),
            "no worktree dir gone → nothing reaped",
        );
        assert_eq!(tracker.global_roots().len(), 2, "both roots survive");
    }

    // ── Reap-log scoping: worktree_contributor_session_id ─────────────────
    //
    // The reaper task has no session span, so it recovers the contributing
    // session id from the `worktree:{session_id}:{path}` key to scope its reap
    // log under the same firehose shard as the mount. This is the parse the
    // `session_id` field on that log feeds from.

    #[test]
    fn worktree_contributor_session_id_extracts_the_session() {
        assert_eq!(
            worktree_contributor_session_id(
                "worktree:dc1bdd1b-7b18-4ab8-a42a-0dbea87a271d:/home/mark/wt"
            ),
            Some("dc1bdd1b-7b18-4ab8-a42a-0dbea87a271d"),
            "the session id between the first two colons is recovered",
        );
    }

    #[test]
    fn worktree_contributor_session_id_stops_at_first_path_colon() {
        // A path may itself contain a colon; only the first segment is the id.
        assert_eq!(
            worktree_contributor_session_id("worktree:sess-a:/odd:path/wt"),
            Some("sess-a"),
            "only the segment before the first path colon is the session id",
        );
    }

    #[test]
    fn worktree_contributor_session_id_rejects_malformed_keys() {
        assert_eq!(
            worktree_contributor_session_id("mcp:1"),
            None,
            "a non-worktree contributor has no worktree session id",
        );
        assert_eq!(
            worktree_contributor_session_id("worktree:/no-session-segment"),
            None,
            "a key without a session:path split yields no id",
        );
        assert_eq!(
            worktree_contributor_session_id("worktree::/empty-session"),
            None,
            "an empty session segment is not a usable scope",
        );
    }

    #[test]
    fn running_background_agent_ids_reads_running_tasks_from_host_payload() {
        // misc 151 D-2: the nag skips a worktree whose owning agent is still
        // running in the stop payload's `background_tasks`.
        let raw = serde_json::json!({
            "host_payload": {
                "hook_event_name": "Stop",
                "background_tasks": [
                    { "id": "sub-1", "status": "running" },
                    { "id": "sub-2", "status": "completed" },
                ],
            },
        });
        let running = running_background_agent_ids(&raw);
        assert_eq!(running, vec!["sub-1".to_string()], "only running tasks");
    }

    #[test]
    fn running_background_agent_ids_empty_without_tasks() {
        let raw = serde_json::json!({ "host_payload": { "hook_event_name": "Stop" } });
        assert!(running_background_agent_ids(&raw).is_empty());
    }

    #[test]
    fn worktree_registry_mark_nagged_is_once_per_worktree() {
        // The lingering nag fires once per worktree per daemon lifetime (D-2).
        let registry = WorktreeRegistry::new();
        let wt = PathBuf::from("/state/catenary/worktrees/agents/s/a");
        assert!(registry.mark_nagged(&wt), "first nag marks");
        assert!(!registry.mark_nagged(&wt), "second nag is suppressed");
    }

    // ── Companion roots (workstream 29) ──────────────────────────────────
    //
    // These exercise the `on_roots_changed` seam: the callback recomputes
    // `expand_companions(declared, rules)` and `set_roots`-REPLACEs the
    // `mcp:{fd}` set on every change. Driving the tracker the same way the
    // callback does proves companions ride `global_roots`, track add/remove
    // for free, and refcount across connections.

    #[test]
    fn companion_rides_global_roots_and_tracks_add_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let foo = base.join("Foo");
        let foo_internal = base.join("FooInternal");
        let bar = base.join("Bar");
        std::fs::create_dir(&foo).expect("mkdir Foo");
        std::fs::create_dir(&foo_internal).expect("mkdir FooInternal");
        std::fs::create_dir(&bar).expect("mkdir Bar");

        let rules = crate::companions::CompanionRules::from_pairs([("*", "{root}Internal")]);
        let tracker = RootTracker::new();

        // Declared = [Foo] → companion FooInternal joins the global set.
        tracker.set_roots("mcp:1", expand_companions(vec![foo.clone()], &rules));
        let global = tracker.global_roots();
        assert!(global.contains(&foo));
        assert!(global.contains(&foo_internal), "companion mounted");
        assert!(!global.contains(&bar));

        // Client adds Bar (no Internal sibling): recompute over the full set.
        tracker.set_roots(
            "mcp:1",
            expand_companions(vec![foo.clone(), bar.clone()], &rules),
        );
        let global = tracker.global_roots();
        assert!(global.contains(&bar));
        assert!(
            global.contains(&foo_internal),
            "Foo's companion survives add"
        );

        // Client removes Foo: recompute over [Bar] drops Foo's companion with
        // no provenance bookkeeping.
        tracker.set_roots("mcp:1", expand_companions(vec![bar.clone()], &rules));
        let global = tracker.global_roots();
        assert!(global.contains(&bar));
        assert!(!global.contains(&foo), "removed root gone");
        assert!(
            !global.contains(&foo_internal),
            "removed root's companion gone via recompute",
        );
    }

    #[test]
    fn shared_companion_refcounts_across_connections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let foo = base.join("Foo");
        let foo_internal = base.join("FooInternal");
        std::fs::create_dir(&foo).expect("mkdir Foo");
        std::fs::create_dir(&foo_internal).expect("mkdir FooInternal");

        let rules = crate::companions::CompanionRules::from_pairs([("*", "{root}Internal")]);
        let tracker = RootTracker::new();

        // Two connections both declare Foo → both derive FooInternal.
        tracker.set_roots("mcp:1", expand_companions(vec![foo.clone()], &rules));
        tracker.set_roots("mcp:2", expand_companions(vec![foo], &rules));
        assert_eq!(
            tracker.refcount(&foo_internal),
            2,
            "companion shared by both"
        );

        // One disconnects: companion survives for the other.
        tracker.remove_contributor("mcp:1");
        assert_eq!(tracker.refcount(&foo_internal), 1);
        assert!(tracker.global_roots().contains(&foo_internal));

        // Last disconnect drops the whole set, companion included.
        tracker.remove_contributor("mcp:2");
        assert_eq!(tracker.refcount(&foo_internal), 0);
        assert!(tracker.global_roots().is_empty());
    }

    /// Build a `file://` MCP root for `path`.
    fn mcp_root(path: &Path) -> crate::mcp::Root {
        crate::mcp::Root {
            uri: format!("file://{}", path.display()),
            name: None,
        }
    }

    #[test]
    fn companion_expanded_roots_off_by_default() {
        // The exact glue the `on_roots_changed` callback runs: with a default
        // config (no `[roots.companions]`), expansion is identity over the
        // client's declared roots.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let foo = base.join("Foo");
        let foo_internal = base.join("FooInternal");
        std::fs::create_dir(&foo).expect("mkdir Foo");
        std::fs::create_dir(&foo_internal).expect("mkdir FooInternal");

        let config = crate::config::Config::default();
        let out = companion_expanded_roots(&[mcp_root(&foo)], &config);

        assert_eq!(out, vec![foo], "no rules ⇒ declared roots only");
        assert!(
            !out.contains(&foo_internal),
            "companion must NOT mount when the feature is off",
        );
    }

    #[test]
    fn companion_expanded_roots_reads_session_config() {
        // With `[roots.companions]` on the config, the callback glue derives the
        // companion from the client's declared root URIs.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let foo = base.join("Foo");
        let foo_internal = base.join("FooInternal");
        std::fs::create_dir(&foo).expect("mkdir Foo");
        std::fs::create_dir(&foo_internal).expect("mkdir FooInternal");

        let config = crate::config::Config {
            roots: Some(crate::config::RootsConfig {
                companions: Some(crate::companions::CompanionRules::from_pairs([(
                    "*",
                    "{root}Internal",
                )])),
            }),
            ..crate::config::Config::default()
        };
        let out = companion_expanded_roots(&[mcp_root(&foo)], &config);

        assert_eq!(
            out,
            vec![foo, foo_internal],
            "declared root plus its derived companion",
        );
    }

    // ── Subagent worktree auto-mount predicate (workstream 30, ticket 03) ─
    //
    // These exercise `worktree_to_auto_mount` — the pure predicate that decides
    // whether a worktree should mount — which the `SubagentStart`
    // mount handler drives on top of (feeding it the subagent's `cwd`). Mirrors
    // `companions::canonical_project_root_linked_worktree_is_main` for the
    // on-disk worktree layout.

    /// Builds a main checkout + one linked worktree on disk, returning
    /// `(canonical_project_root, worktree_root)`.
    ///
    /// Layout mirrors git's: `<base>/project/.git/worktrees/wt/commondir` → `../..`
    /// and `<base>/checkout/.git` is a file pointing at the worktree gitdir.
    fn linked_worktree_layout(base: &Path) -> (PathBuf, PathBuf) {
        let project = base.join("project");
        let wt_gitdir = project.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&wt_gitdir).expect("mkdir worktree gitdir");
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").expect("write commondir");

        let checkout = base.join("checkout");
        std::fs::create_dir(&checkout).expect("mkdir checkout");
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .expect("write .git file");

        (project, checkout)
    }

    #[test]
    fn auto_mount_worktree_when_canonical_root_tracked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);
        let file = worktree.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir src");
        std::fs::write(&file, "").expect("write file");

        // The session already tracks the canonical project root.
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![project.clone()]);
        let roots: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();

        // The predicate selects the *worktree*, never the canonical root.
        let mount = worktree_to_auto_mount(&file, &roots).expect("worktree should auto-mount");
        assert_eq!(
            mount, worktree,
            "mounts the worktree, not the canonical root"
        );

        // Drive the `worktree:{sid}:{path}` wiring the SubagentStart handler uses.
        let contributor = format!("worktree:sid-1:{}", mount.display());
        tracker.set_roots(&contributor, vec![mount]);
        let global = tracker.global_roots();
        assert!(global.contains(&worktree), "worktree mounted");
        assert!(
            global.contains(&project),
            "canonical root still tracked (its own contributor)",
        );
        // The worktree rides ONLY the worktree key — not the canonical root's set.
        assert_eq!(
            tracker.refcount(&worktree),
            1,
            "worktree held by worktree key only"
        );
        let sources: Vec<String> = tracker
            .list_roots()
            .into_iter()
            .find(|(p, _)| p == &worktree)
            .map(|(_, s)| s)
            .expect("worktree present");
        assert_eq!(sources, vec![contributor]);
    }

    #[test]
    fn no_auto_mount_when_canonical_root_untracked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let (_project, worktree) = linked_worktree_layout(&base);
        let file = worktree.join("src.rs");
        std::fs::write(&file, "").expect("write file");

        // An *unrelated* repo is tracked — not this worktree's canonical root.
        let unrelated = base.join("unrelated");
        std::fs::create_dir(&unrelated).expect("mkdir unrelated");
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![unrelated]);
        let roots: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();

        assert_eq!(
            worktree_to_auto_mount(&file, &roots),
            None,
            "an edit in a worktree of an untracked project must not auto-mount",
        );
    }

    #[test]
    fn no_auto_mount_for_main_agent_in_tracked_root() {
        // The main agent editing a plain checkout that is already a tracked root:
        // the worktree IS its canonical root and is already tracked, so the
        // "not already tracked" clause rejects it — no spurious mount.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        let file = root.join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir src");
        std::fs::write(&file, "").expect("write file");

        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![root]);
        let roots: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();

        assert_eq!(
            worktree_to_auto_mount(&file, &roots),
            None,
            "main agent editing inside an already-tracked checkout is a no-op",
        );
    }

    #[test]
    fn no_auto_mount_when_worktree_already_tracked() {
        // Idempotency: the worktree is already a tracked root (e.g. a prior
        // SubagentStart mounted it) — even though its canonical root is also
        // tracked, a second spawn signal must not re-trigger.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);
        let file = worktree.join("again.rs");
        std::fs::write(&file, "").expect("write file");

        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![project]);
        tracker.set_roots(
            &format!("worktree:sid-1:{}", worktree.display()),
            vec![worktree],
        );
        let roots: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();

        assert_eq!(
            worktree_to_auto_mount(&file, &roots),
            None,
            "an already-mounted worktree must not re-trigger a mount",
        );
    }

    #[test]
    fn no_auto_mount_for_file_outside_any_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let file = base.join("loose.rs");
        std::fs::write(&file, "").expect("write file");

        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![base.join("project")]);
        let roots: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();

        assert_eq!(
            worktree_to_auto_mount(&file, &roots),
            None,
            "a file outside any git checkout has no worktree to mount",
        );
    }

    #[test]
    fn auto_mount_out_of_tree_cache_dir_worktree() {
        // misc 144: the `WorktreeCreate` hook relocates the worktree OUTSIDE the
        // repo tree, under a cache-dir path. A git worktree records its upstream
        // through a `.git` *file* (`gitdir: <repo>/.git/worktrees/<name>`), not
        // its filesystem location, so the mount predicate must still resolve the
        // canonical project root and mount the worktree — wherever it physically
        // lives. This is the structural property bug 53's relocation relies on.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");

        // Main project with a linked-worktree metadata dir.
        let project = base.join("project");
        let wt_gitdir = project.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&wt_gitdir).expect("mkdir worktree gitdir");
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").expect("write commondir");

        // The worktree lives far from the repo, under a cache-dir-style path
        // (`<cache>/catenary/worktrees/<flattened-repo>-<id>`).
        let worktree = base
            .join("cache")
            .join("catenary")
            .join("worktrees")
            .join("-p-project-abc123");
        std::fs::create_dir_all(&worktree).expect("mkdir out-of-tree worktree");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .expect("write .git file");
        let file = worktree.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir src");
        std::fs::write(&file, "").expect("write file");

        // The session tracks the canonical project root.
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![project]);
        let roots: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();

        let mount = worktree_to_auto_mount(&file, &roots)
            .expect("out-of-tree worktree of a tracked project should auto-mount");
        assert_eq!(
            mount, worktree,
            "mounts the out-of-tree worktree, not the canonical root",
        );
    }

    // ── Subagent worktree lifecycle: SubagentStart / WorktreeRemove ──────
    //
    // End-to-end dispatch tests for the workstream-30 ticket-03 re-bracket: the
    // worktree root is mounted at `subagent-start/mount-worktree` and torn down
    // at `worktree-remove/unmount-worktree`, keyed by
    // `worktree:{session_id}:{canonical path}`. They drive the live dispatch
    // path over the IPC socket (via `hook_roundtrip`) and inspect the resulting
    // `RootTracker` through a `tool/roots-ls` round-trip — fully black-box.
    //
    // The canonical project root is seeded into the tracker via `tool/roots-add`
    // (the `hook` contributor); the auto-mount predicate only needs it present
    // in the global root set.

    /// Round-trip `tool/roots-ls` and return the `(path, sources)` pairs.
    async fn roots_ls(ipc_path: &Path) -> Vec<(String, Vec<String>)> {
        let resp = hook_roundtrip(ipc_path, &serde_json::json!({"method": "tool/roots-ls"})).await;
        let json: serde_json::Value = serde_json::from_str(&resp).expect("roots-ls json");
        json.get("roots")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|e| {
                        let path = e
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .expect("path")
                            .to_string();
                        let sources = e
                            .get("sources")
                            .and_then(serde_json::Value::as_array)
                            .map(|s| {
                                s.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (path, sources)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_start_mounts_worktree_when_canonical_root_tracked() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Seed the canonical project root (what authorizes the mount).
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;

        // SubagentStart with cwd = the linked worktree → mount under
        // worktree:{sid}:{path}.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;

        let roots = roots_ls(&ipc_path).await;
        let entry = roots
            .iter()
            .find(|(p, _)| Path::new(p) == worktree)
            .expect("worktree should be mounted");
        assert_eq!(
            entry.1,
            vec![format!("worktree:sess-1:{}", worktree.display())],
            "worktree held by the worktree:{{sid}}:{{path}} contributor",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_start_self_scopes_no_mount_for_tracked_root() {
        // Explore/Plan self-scoping: a non-isolated subagent spawns with cwd =
        // an already-tracked root (the main checkout). `worktree_to_auto_mount`
        // returns None (already tracked AND canonical == cwd), so NO new
        // contributor is created.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": root.display().to_string(),
            }),
        )
        .await;

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": root.display().to_string(),
            }),
        )
        .await;

        let roots = roots_ls(&ipc_path).await;
        // Only the seeded root remains, under the `hook` contributor — no
        // worktree:* contributor was created.
        assert_eq!(roots.len(), 1, "no new root mounted: {roots:?}");
        assert_eq!(roots[0].0, root.display().to_string());
        assert_eq!(roots[0].1, vec!["hook".to_string()]);
        assert!(
            !roots
                .iter()
                .any(|(_, s)| s.iter().any(|c| c.starts_with("worktree:"))),
            "no worktree:* contributor for an already-tracked-root cwd",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worktree_remove_tears_down_mounted_worktree() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;
        assert!(
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "precondition: worktree mounted",
        );

        // WorktreeRemove with the same worktree_path → teardown. The dir still
        // exists, so canonicalize agrees with the mount key by construction.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "worktree-remove/unmount-worktree",
                "session_id": "sess-1",
                "worktree_path": worktree.display().to_string(),
            }),
        )
        .await;

        let roots = roots_ls(&ipc_path).await;
        assert!(
            !roots.iter().any(|(p, _)| Path::new(p) == worktree),
            "worktree torn down at WorktreeRemove: {roots:?}",
        );
        // The seeded project root (other contributor) is untouched.
        assert!(
            roots.iter().any(|(p, _)| Path::new(p) == project),
            "the project root (mcp/hook contributor) survives teardown",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_worktree_mount_remove_round_trip() {
        // Full round-trip: mount then remove. The worktree root is gone after
        // removal; a separate project root contributor is untouched throughout.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;

        // Mount.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;
        let mounted = roots_ls(&ipc_path).await;
        assert!(mounted.iter().any(|(p, _)| Path::new(p) == worktree));
        assert!(mounted.iter().any(|(p, _)| Path::new(p) == project));

        // Remove.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "worktree-remove/unmount-worktree",
                "session_id": "sess-1",
                "worktree_path": worktree.display().to_string(),
            }),
        )
        .await;
        let after = roots_ls(&ipc_path).await;
        assert!(
            !after.iter().any(|(p, _)| Path::new(p) == worktree),
            "worktree gone after round-trip",
        );
        assert_eq!(after.len(), 1, "only the project root remains: {after:?}");
        assert_eq!(after[0].0, project.display().to_string());

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_tears_down_mounted_worktree() {
        // misc 150 hardening: a SubagentStop reaches the daemon as
        // `post-agent/require-release`. With `agent_id` present at both mount and
        // stop, the mount is IDENTITY-keyed (`worktree:{sid}:{agent_id}`) and the
        // reap rebuilds that key from identity alone — no path needed. The reap is
        // a pure side effect while the require-release contract still allows.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;
        // Mount WITH an agent id → identity-keyed contributor.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;
        let mounted = roots_ls(&ipc_path).await;
        let entry = mounted
            .iter()
            .find(|(p, _)| Path::new(p) == worktree)
            .expect("precondition: worktree mounted");
        assert_eq!(
            entry.1,
            vec!["worktree:sess-1:sub-1".to_string()],
            "the mount is identity-keyed, not path-keyed",
        );

        // SubagentStop carrying the same identity — reaps by identity alone.
        let resp = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "stop_hook_active": false,
                "host_payload": {
                    "hook_event_name": "SubagentStop",
                    "cwd": worktree.display().to_string(),
                },
            }),
        )
        .await;
        // Require-release contract intact: a subagent with no editing debt is
        // allowed to stop — empty response, never a block. The teardown is a
        // pure side effect (decision 029).
        assert!(
            resp.trim().is_empty(),
            "require-release still allows the stop (teardown is invisible): {resp:?}",
        );

        let roots = roots_ls(&ipc_path).await;
        assert!(
            !roots.iter().any(|(p, _)| Path::new(p) == worktree),
            "worktree torn down at SubagentStop: {roots:?}",
        );
        // The seeded project root (other contributor) is untouched.
        assert!(
            roots.iter().any(|(p, _)| Path::new(p) == project),
            "the project root (hook contributor) survives teardown",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_stop_event_leaves_worktree_mounted() {
        // A `Stop` event (the parent agent finishing, not a subagent) shares the
        // `post-agent/require-release` method but must NOT reap the worktree —
        // only `hook_event_name == "SubagentStop"` triggers the teardown.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;
        assert!(
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "precondition: worktree mounted",
        );

        // Same worktree cwd, but a `Stop` event (the parent finishing) — no
        // teardown, whatever the cwd. Only `SubagentStop` triggers the reap.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "",
                "stop_hook_active": false,
                "host_payload": {
                    "hook_event_name": "Stop",
                    "cwd": worktree.display().to_string(),
                },
            }),
        )
        .await;

        assert!(
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "worktree still mounted after a Stop (non-SubagentStop) event",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_leaves_non_worktree_root_mounted() {
        // A SubagentStop whose `cwd` is a pinned (non-worktree) root — a
        // non-isolated subagent running in the main checkout — matches no
        // `worktree:{session}:{cwd}` contributor, so the reap is a no-op and the
        // pinned root survives. Only the worktree contributor class is touched.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let pinned = base.join("pinned");
        std::fs::create_dir(&pinned).expect("mkdir pinned");

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Pin the root under the `hook` contributor (never a `worktree:*` key).
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": pinned.display().to_string(),
            }),
        )
        .await;
        assert!(
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == pinned),
            "precondition: pinned root tracked",
        );

        // SubagentStop with the pinned root as cwd — a no-op for the reap.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "stop_hook_active": false,
                "host_payload": {
                    "hook_event_name": "SubagentStop",
                    "cwd": pinned.display().to_string(),
                },
            }),
        )
        .await;

        let roots = roots_ls(&ipc_path).await;
        assert!(
            roots.iter().any(|(p, _)| Path::new(p) == pinned),
            "pinned non-worktree root survives a SubagentStop: {roots:?}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_outcome_gate_blocks_then_reaps() {
        // Outcome gate (misc 150 hardening): SubagentStop has two sequenced jobs.
        // While there is undelivered editing debt, require-release BLOCKS (the
        // agent is not stopping — it is about to run diagnostics in the worktree),
        // so the reap must NOT run and the root stays warm. The `stop_hook_active`
        // retry then ALLOWS the stop and the reap runs.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;

        // Give the subagent undelivered editing debt: start editing then record a
        // covered edit on its per-session router (mirrors the editing-state tests).
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "pre-tool/editing-start",
                "agent_id": "sub-1",
                "session_id": "sess-1",
            }),
        )
        .await;
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let sessions = ctx.sessions.lock().expect("lock");
            let router = Arc::clone(&sessions.get("sess-1").expect("sess-1").router);
            drop(sessions);
            router.session.editing.record_covered_edit(
                Some("sess-1"),
                "sub-1",
                std::path::PathBuf::from("/src/main.rs"),
            );
        }

        // First SubagentStop: debt undelivered → BLOCK → NO reap.
        let resp = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "stop_hook_active": false,
                "host_payload": {
                    "hook_event_name": "SubagentStop",
                    "cwd": worktree.display().to_string(),
                },
            }),
        )
        .await;
        let envelope: crate::hook::HookResponseEnvelope =
            serde_json::from_str(resp.trim()).expect("parse block response");
        assert!(
            matches!(envelope.result, Some(crate::hook::HookResult::Block(_))),
            "require-release blocks while debt is undelivered: {envelope:?}",
        );
        assert!(
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "a blocked stop leaves the worktree mounted (servers stay warm for diagnostics)",
        );

        // Retry with `stop_hook_active` → the stop is ALLOWED → the reap runs.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "stop_hook_active": true,
                "host_payload": {
                    "hook_event_name": "SubagentStop",
                    "cwd": worktree.display().to_string(),
                },
            }),
        )
        .await;
        assert!(
            !roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "the allowed retry reaps the worktree root",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_foreign_no_agent_id_path_keyed_round_trip() {
        // Foreign/legacy worktree: no `agent_id` at mount OR stop. The mount is
        // path-keyed (`worktree:{sid}:{path}`) and the reap resolves via the cwd
        // route (the enclosing worktree root of the stop cwd).
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;
        // Mount WITHOUT an agent id → path-keyed contributor.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;
        let mounted = roots_ls(&ipc_path).await;
        let entry = mounted
            .iter()
            .find(|(p, _)| Path::new(p) == worktree)
            .expect("precondition: worktree mounted");
        assert_eq!(
            entry.1,
            vec![format!("worktree:sess-1:{}", worktree.display())],
            "no agent id → path-keyed contributor",
        );

        // SubagentStop WITHOUT an agent id → cwd route reaps the path-keyed mount.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "",
                "stop_hook_active": false,
                "host_payload": {
                    "hook_event_name": "SubagentStop",
                    "cwd": worktree.display().to_string(),
                },
            }),
        )
        .await;
        assert!(
            !roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "path-keyed foreign worktree still reaps at stop via the cwd route",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_cwd_fallback_reaps_from_subdirectory() {
        // The cwd fallback resolves the ENCLOSING worktree root, never an exact
        // match on the raw cwd: a final `cd` into a subdirectory of the worktree
        // (the host's carry-over default) still reaps a path-keyed mount.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, worktree) = linked_worktree_layout(&base);
        let subdir = worktree.join("src").join("deep");
        std::fs::create_dir_all(&subdir).expect("mkdir subdir");

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;
        // Path-keyed mount (no agent id).
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": worktree.display().to_string(),
            }),
        )
        .await;
        assert!(
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "precondition: worktree mounted",
        );

        // Stop reports a SUBDIRECTORY of the worktree as cwd, no agent id.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "",
                "stop_hook_active": false,
                "host_payload": {
                    "hook_event_name": "SubagentStop",
                    "cwd": subdir.display().to_string(),
                },
            }),
        )
        .await;
        assert!(
            !roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "enclosing-root resolution reaps even from a worktree subdirectory",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_prefers_registry_over_cwd_on_divergence() {
        // When the identity route and the cwd route disagree, the identity/registry
        // route wins: the stop reports worktree B's path as cwd, but the agent's
        // identity mount is worktree A — so A is reaped and B survives (cwd was not
        // used for the reap).
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project_a, worktree_a) = linked_worktree_layout(&base.join("a"));
        let (project_b, worktree_b) = linked_worktree_layout(&base.join("b"));

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        for project in [&project_a, &project_b] {
            let _ = hook_roundtrip(
                &ipc_path,
                &serde_json::json!({
                    "method": "tool/roots-add",
                    "path": project.display().to_string(),
                }),
            )
            .await;
        }

        // Identity-key worktree A under sub-1; register it in the daemon registry.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "cwd": worktree_a.display().to_string(),
            }),
        )
        .await;
        let meta = crate::worktree_create::WorktreeMeta {
            worktree: worktree_a.clone(),
            source_repo: project_a.clone(),
            base_commit: "deadbeef".to_string(),
            branch: "agent-sub-1".to_string(),
            name: "agent-sub-1".to_string(),
            agent_id: Some("sub-1".to_string()),
            session_id: "sess-1".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: "agent".to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        };
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "worktree-create/log-payload",
                "session_id": "sess-1",
                "worktree_meta": meta,
            }),
        )
        .await;

        // Mount worktree B under a different (path-keyed) contributor so we can
        // watch it survive.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": worktree_b.display().to_string(),
            }),
        )
        .await;

        // SubagentStop: identity is sub-1 (→ worktree A) but cwd is worktree B.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "post-agent/require-release",
                "session_id": "sess-1",
                "agent_id": "sub-1",
                "stop_hook_active": false,
                "host_payload": {
                    "hook_event_name": "SubagentStop",
                    "cwd": worktree_b.display().to_string(),
                },
            }),
        )
        .await;

        let roots = roots_ls(&ipc_path).await;
        assert!(
            !roots.iter().any(|(p, _)| Path::new(p) == worktree_a),
            "the identity worktree (A) is reaped: {roots:?}",
        );
        assert!(
            roots.iter().any(|(p, _)| Path::new(p) == worktree_b),
            "the cwd worktree (B) survives — cwd was not used for the reap: {roots:?}",
        );

        shutdown.cancel();
    }

    #[test]
    fn worktree_registry_rehydrates_from_sidecars() {
        // A fresh registry (what `with_session` builds on daemon start) rebuilds
        // the identity→path map by scanning the agents subtree for sidecars — a
        // daemon restart loses nothing durable (misc 150).
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents = tmp.path().join("agents");
        let wt = agents.join("sess-1").join("abc");
        std::fs::create_dir_all(&wt).expect("mkdir worktree");
        let meta = crate::worktree_create::WorktreeMeta {
            worktree: wt.clone(),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "agent-abc".to_string(),
            name: "agent-abc".to_string(),
            agent_id: Some("abc".to_string()),
            session_id: "sess-1".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: "agent".to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        };
        crate::worktree_create::write_sidecar(&meta).expect("write sidecar");

        let registry = WorktreeRegistry::new();
        registry.rehydrate(crate::worktree_create::scan_sidecars(&agents));
        assert_eq!(registry.len(), 1, "one registration rehydrated");
        assert_eq!(
            registry.path_for_identity("sess-1", "abc"),
            Some(wt),
            "identity→path map rebuilt from the sidecar",
        );
        assert_eq!(
            registry.path_for_identity("sess-1", "other"),
            None,
            "an unregistered identity is absent",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_end_sweeps_only_that_sessions_worktree_roots() {
        // Leak backstop (graceful tier): `session-end/cleanup` for session 1
        // removes every `worktree:sess-1:*` contributor while session 2's
        // worktree and the project roots survive (the sweep is keyed by the
        // `session_id` baked into the contributor key).
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");

        // Two independent main-checkout + linked-worktree layouts, one per
        // session, in distinct subdirs.
        let (project_a, worktree_a) = linked_worktree_layout(&base.join("a"));
        let (project_b, worktree_b) = linked_worktree_layout(&base.join("b"));

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Seed both canonical project roots (what authorizes each mount).
        for project in [&project_a, &project_b] {
            let _ = hook_roundtrip(
                &ipc_path,
                &serde_json::json!({
                    "method": "tool/roots-add",
                    "path": project.display().to_string(),
                }),
            )
            .await;
        }

        // Mount worktree_a under sess-1 and worktree_b under sess-2.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-1",
                "cwd": worktree_a.display().to_string(),
            }),
        )
        .await;
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "subagent-start/mount-worktree",
                "session_id": "sess-2",
                "cwd": worktree_b.display().to_string(),
            }),
        )
        .await;

        let before = roots_ls(&ipc_path).await;
        assert!(
            before.iter().any(|(p, _)| Path::new(p) == worktree_a),
            "precondition: sess-1 worktree mounted",
        );
        assert!(
            before.iter().any(|(p, _)| Path::new(p) == worktree_b),
            "precondition: sess-2 worktree mounted",
        );

        // Graceful end for sess-1 only — sweeps worktree:sess-1:* (no
        // WorktreeRemove was sent for it).
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "session-end/cleanup",
                "session_id": "sess-1",
            }),
        )
        .await;

        let after = roots_ls(&ipc_path).await;
        assert!(
            !after.iter().any(|(p, _)| Path::new(p) == worktree_a),
            "sess-1's worktree root swept at session end: {after:?}",
        );
        assert!(
            after.iter().any(|(p, _)| Path::new(p) == worktree_b),
            "sess-2's worktree (different session prefix) survives: {after:?}",
        );
        assert!(
            after.iter().any(|(p, _)| Path::new(p) == project_a),
            "project_a (hook contributor) survives the worktree sweep",
        );
        assert!(
            after.iter().any(|(p, _)| Path::new(p) == project_b),
            "project_b (hook contributor) survives",
        );

        shutdown.cancel();
    }

    // ── Worktree dir-deletion teardown (workstream 30, ticket 05) ─────────
    //
    // `WorktreeRemove` never fires for git worktrees (the host runs
    // `git worktree remove` itself), so the prompt teardown trigger is the
    // bounded directory-deletion watch. These tests cover the *reap* the watch
    // reaper, the GC, and the `SessionEnd` sweep all share — `remove_contributor`
    // + watch `unregister` + the reduced-union sync — and its idempotence across
    // those paths. The real-FS deletion→channel half (the OS watch firing on a
    // `remove_dir_all`) is covered by `worktree_watch::tests`'
    // `deletion_emits_contributor_event`; here we drive the tracker + watcher
    // directly so the assertions stay deterministic (no OS-event timing).

    #[test]
    fn worktree_watch_reap_drops_only_the_deleted_root() {
        // The reaper's per-event action (see `spawn_worktree_watch_reaper`): on a
        // `WorktreeDeleted` for one watched worktree, `unregister` its watch and
        // `remove_contributor`, leaving every other root in `global_roots`
        // untouched. Mirrors `reap_missing_worktree_roots_reaps_only_gone_dirs`
        // but for the prompt watch path rather than the hourly GC.
        let (watcher, _rx) = crate::worktree_watch::WorktreeWatcher::new().expect("create watcher");
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");

        let deleted = base.join("agent-deleted");
        let kept = base.join("agent-kept");
        std::fs::create_dir(&deleted).expect("mkdir deleted");
        std::fs::create_dir(&kept).expect("mkdir kept");

        let tracker = RootTracker::new();
        let key_deleted = format!("worktree:sess:{}", deleted.display());
        let key_kept = format!("worktree:sess:{}", kept.display());
        tracker.set_roots(&key_deleted, vec![deleted.clone()]);
        tracker.set_roots(&key_kept, vec![kept.clone()]);
        // A non-worktree contributor — must survive an unrelated reap.
        tracker.set_roots("hook", vec![base.join("project")]);
        watcher.register(&key_deleted, &deleted);
        watcher.register(&key_kept, &kept);

        // The reaper's body for a single `WorktreeDeleted { contributor }`.
        watcher.unregister(&key_deleted);
        tracker.remove_contributor(&key_deleted);

        let global = tracker.global_roots();
        assert!(!global.contains(&deleted), "the deleted worktree is reaped");
        assert!(global.contains(&kept), "the sibling worktree survives");
        assert!(
            global.contains(&base.join("project")),
            "the non-worktree (hook) root is untouched",
        );
        // The reaped contributor's watch is gone; the sibling's remains (shared
        // parent, refcounted down by one).
        assert!(!watcher.is_registered(&key_deleted));
        assert!(watcher.is_registered(&key_kept));
    }

    #[test]
    fn worktree_reap_is_idempotent_across_watch_gc_and_session_end() {
        // The watch reaper, the hourly GC, and the `SessionEnd` sweep can all
        // reap the same `worktree:{sid}:{path}` key without disagreeing: the
        // second (and third) reap is a harmless no-op — `remove_contributor` of
        // an absent key changes nothing, the root set is unchanged, and a repeat
        // `unregister` is a no-op. This is the guarantee that lets the three
        // teardown paths run concurrently (ticket 05).
        let (watcher, _rx) = crate::worktree_watch::WorktreeWatcher::new().expect("create watcher");
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let wt = base.join("agent-x");
        std::fs::create_dir(&wt).expect("mkdir wt");

        let tracker = RootTracker::new();
        let key = format!("worktree:sess:{}", wt.display());
        tracker.set_roots(&key, vec![wt.clone()]);
        tracker.set_roots("hook", vec![base.join("project")]);
        watcher.register(&key, &wt);

        // First reap (e.g. the watch reaper).
        watcher.unregister(&key);
        tracker.remove_contributor(&key);
        let after_first = tracker.global_roots();
        assert!(!after_first.contains(&wt), "reaped on the first pass");
        assert_eq!(
            after_first.len(),
            1,
            "only the unrelated root remains: {after_first:?}",
        );
        assert!(!watcher.is_registered(&key));

        // Second reap via a different path (the GC's `unregister` + the
        // `SessionEnd` sweep's prefix `unregister`) — must change nothing.
        watcher.unregister(&key);
        watcher.unregister_with_prefix("worktree:sess:");
        tracker.remove_contributor(&key);
        // The prefix sweep the `SessionEnd` backstop runs is also a no-op now.
        assert_eq!(
            tracker.remove_contributors_with_prefix("worktree:sess:"),
            0,
            "no worktree:sess:* contributors left to sweep",
        );
        let after_second = tracker.global_roots();
        assert_eq!(
            after_second.len(),
            after_first.len(),
            "the root set is unchanged on the second reap",
        );
        assert!(
            !after_second.contains(&wt),
            "the reaped worktree stays gone on the second reap",
        );
        assert!(!watcher.is_registered(&key));
    }

    #[test]
    fn worktree_mount_race_guard_reaps_dir_gone_at_registration() {
        // Race fast-path: the inline `.exists()` guard in the
        // `subagent-start/mount-worktree` handler. If the worktree dir is removed
        // in the window between the auto-mount predicate canonicalizing it (dir
        // present) and the watch registration, the handler reaps the just-mounted
        // contributor immediately — `unregister` + `remove_contributor` — rather
        // than leaving a root on a dead dir. This test reproduces that exact branch
        // deterministically: mount a contributor, register its watch, delete the
        // dir, then run the guard body.
        //
        // The guard's *inline* timing window can't be forced from a round-trip
        // test (the handler runs synchronously, and `worktree_to_auto_mount`'s
        // `enclosing_worktree_root` requires `.git` *inside* the worktree, so the
        // dir necessarily exists when the predicate authorizes the mount). The
        // OS-event reach of the watch — a real `remove_dir_all` firing the reap —
        // is covered by `worktree_watch::tests::deletion_emits_contributor_event`.
        let (watcher, _rx) = crate::worktree_watch::WorktreeWatcher::new().expect("create watcher");
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let worktree = base.join("agent-raced");

        let tracker = RootTracker::new();
        tracker.set_roots("hook", vec![base.join("project")]);
        let contributor = format!("worktree:sess:{}", worktree.display());

        // Mount + watch (as the handler does on the authorized predicate). The
        // worktree dir is already gone by the time the watch is registered — the
        // race the inline guard exists for. (We delete it before registering so no
        // live OS delete event is in flight, keeping the test deterministic; the
        // real OS-event path is covered by `worktree_watch::tests`.)
        tracker.set_roots(&contributor, vec![worktree.clone()]);
        watcher.register(&contributor, &worktree);
        assert!(tracker.global_roots().contains(&worktree), "mounted");

        // The guard body: `if !worktree.exists() { unregister + remove_contributor }`.
        assert!(!worktree.exists(), "the race-guard precondition holds");
        watcher.unregister(&contributor);
        tracker.remove_contributor(&contributor);

        let global = tracker.global_roots();
        assert!(
            !global.contains(&worktree),
            "the dir-gone worktree is reaped immediately, not left mounted",
        );
        assert!(
            global.contains(&base.join("project")),
            "the project root survives the immediate reap",
        );
        assert!(!watcher.is_registered(&contributor), "the watch is dropped");
    }

    // ── Ephemeral, activity-mounted roots (ephemeral-roots ticket 02) ─────
    //
    // An out-of-root CLI query (grep/glob/diagnostics) mounts the enclosing
    // project root under an `ephemeral:{path}` contributor; the idle reaper
    // tears it down. The pure predicate + idle clock + reaper are driven with an
    // injected `now`/`idle` (no wall-clock sleep — zero-flake doctrine), and the
    // live mount + `pin` upgrade are exercised over the IPC socket.

    /// A minimal marker-ed project: a dir with a `.git` dir and one non-code
    /// file (so no LSP server spawns for it — the tests stay off real servers).
    fn marker_project(base: &Path, name: &str) -> (PathBuf, PathBuf) {
        let project = base.join(name);
        std::fs::create_dir_all(project.join(".git")).expect("mkdir .git");
        let file = project.join("notes.txt");
        std::fs::write(&file, "hello world\n").expect("write file");
        (project, file)
    }

    /// Round-trip `tool/roots-ls` and return `(path, ephemeral)` pairs.
    async fn roots_ls_classes(ipc_path: &Path) -> Vec<(String, bool)> {
        let resp = hook_roundtrip(ipc_path, &serde_json::json!({"method": "tool/roots-ls"})).await;
        let json: serde_json::Value = serde_json::from_str(&resp).expect("roots-ls json");
        json.get("roots")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|e| {
                        let path = e
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .expect("path")
                            .to_string();
                        let ephemeral = e
                            .get("ephemeral")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        (path, ephemeral)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn ephemeral_root_to_mount_detects_enclosing_and_skips_covered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, file) = marker_project(&base, "Lattice");
        let file = file.canonicalize().expect("canonicalize file");

        // Outside every tracked root, enclosing `.git` detectable → mount it.
        let empty = HashSet::new();
        assert_eq!(
            ephemeral_root_to_mount(&file, &empty),
            Some(project.clone()),
            "an out-of-root file mounts its enclosing project root",
        );

        // Already inside a tracked root → covered, no mount.
        let tracked: HashSet<PathBuf> = std::iter::once(project).collect();
        assert_eq!(
            ephemeral_root_to_mount(&file, &tracked),
            None,
            "a file under a tracked root is already covered",
        );

        // No enclosing `.git` → no mount (the ticket-01 fallback still answers).
        let orphan = base.join("loose.txt");
        std::fs::write(&orphan, "x").expect("write");
        let orphan = orphan.canonicalize().expect("canon");
        assert_eq!(ephemeral_root_to_mount(&orphan, &empty), None);
    }

    #[test]
    fn ephemeral_mounts_touch_covering_and_expiry() {
        let mounts = EphemeralMounts::new();
        let root = PathBuf::from("/proj/eph");
        let t0 = Instant::now();
        mounts.touch(&root, t0);

        // Fresh → not idle.
        assert!(mounts.expired(t0, Duration::from_secs(1)).is_empty());

        // A covering activity (a file under the root) refreshes the clock.
        let later = t0 + Duration::from_mins(5);
        mounts.touch_covering(&root.join("src/x.rs"), later);
        assert!(
            mounts.expired(later, Duration::from_secs(1)).is_empty(),
            "touch_covering refreshed the root — not idle at `later`",
        );

        // An unrelated path does NOT refresh it → idle past the threshold.
        let even_later = later + Duration::from_mins(10);
        mounts.touch_covering(Path::new("/other/y.rs"), even_later);
        assert_eq!(
            mounts.expired(even_later, Duration::from_secs(1)),
            vec![root.clone()],
            "an unrelated path left the clock stale — now idle",
        );

        // Removal drops the clock entry entirely.
        mounts.remove(&root);
        assert!(
            mounts
                .expired(even_later, Duration::from_secs(1))
                .is_empty()
        );
    }

    #[test]
    fn ephemeral_mounts_idle_remaining_counts_down() {
        let mounts = EphemeralMounts::new();
        let root = PathBuf::from("/proj/eph");
        let t0 = Instant::now();
        mounts.touch(&root, t0);

        // Full band remaining at t0; elapsed time subtracts; a past deadline
        // saturates to zero rather than underflowing. (Non-whole-minute second
        // counts sidestep the readability lint while keeping the arithmetic
        // obvious.)
        assert_eq!(
            mounts.idle_remaining(&root, t0, Duration::from_secs(90)),
            Some(Duration::from_secs(90)),
        );
        assert_eq!(
            mounts.idle_remaining(&root, t0 + Duration::from_secs(40), Duration::from_secs(90)),
            Some(Duration::from_secs(50)),
        );
        assert_eq!(
            mounts.idle_remaining(
                &root,
                t0 + Duration::from_secs(500),
                Duration::from_secs(90)
            ),
            Some(Duration::ZERO),
        );
        // An untracked root carries no clock.
        assert_eq!(
            mounts.idle_remaining(Path::new("/proj/other"), t0, Duration::from_secs(90)),
            None,
        );
    }

    #[test]
    fn root_board_surfaces_sources_and_idle_remaining() {
        use crate::state_snapshot::RootBoard;

        let tracker = RootTracker::new();
        let mounts = EphemeralMounts::new();

        let pinned = PathBuf::from("/p/Catenary");
        let ephemeral = PathBuf::from("/p/Scratch");
        tracker.add_roots("hook", std::slice::from_ref(&pinned));
        tracker.add_roots("mcp:3", std::slice::from_ref(&pinned));
        tracker.add_roots(
            &ephemeral_contributor(&ephemeral),
            std::slice::from_ref(&ephemeral),
        );
        // Only the ephemeral root carries an idle clock.
        mounts.touch(&ephemeral, Instant::now());

        let board = RootBoardImpl {
            tracker,
            ephemeral_mounts: mounts,
        };
        // `list_roots` sorts by path: Catenary before Scratch.
        let roots = board.roots();
        assert_eq!(roots.len(), 2);

        let cat = &roots[0];
        assert_eq!(cat.path, "/p/Catenary");
        assert!(!cat.ephemeral, "pinned root");
        assert_eq!(
            cat.sources,
            vec!["hook".to_string(), "mcp:3".to_string()],
            "the full contributor classes, not just the ephemeral bool",
        );
        assert!(
            cat.idle_remaining_secs.is_none(),
            "a pinned root has no idle clock",
        );

        let scratch = &roots[1];
        assert_eq!(scratch.path, "/p/Scratch");
        assert!(scratch.ephemeral, "ephemeral root");
        assert_eq!(scratch.sources, vec![ephemeral_contributor(&ephemeral)]);
        let remaining = scratch
            .idle_remaining_secs
            .expect("an ephemeral root carries an idle-remaining figure");
        assert!(
            remaining <= EPHEMERAL_ROOT_IDLE_TIMEOUT.as_secs()
                && remaining + 5 >= EPHEMERAL_ROOT_IDLE_TIMEOUT.as_secs(),
            "idle-remaining sits just under the full band: {remaining}",
        );
    }

    #[test]
    fn reap_idle_ephemeral_roots_expires_only_idle() {
        let tracker = RootTracker::new();
        let mounts = EphemeralMounts::new();
        let idle_root = PathBuf::from("/proj/idle");
        let fresh_root = PathBuf::from("/proj/fresh");
        tracker.set_roots(&ephemeral_contributor(&idle_root), vec![idle_root.clone()]);
        tracker.set_roots(
            &ephemeral_contributor(&fresh_root),
            vec![fresh_root.clone()],
        );

        // Both mounted at t0; only `fresh_root` is refreshed at `later`. Using
        // addition (never subtraction) keeps the Instants panic-free regardless
        // of the monotonic clock's origin.
        let t0 = Instant::now();
        mounts.touch(&idle_root, t0);
        mounts.touch(&fresh_root, t0);
        let later = t0 + Duration::from_mins(10);
        mounts.touch(&fresh_root, later);

        let expired = reap_idle_ephemeral_roots(&tracker, &mounts, later, Duration::from_mins(5));
        assert_eq!(
            expired,
            vec![idle_root.clone()],
            "only the idle root expires"
        );

        let global = tracker.global_roots();
        assert!(!global.contains(&idle_root), "idle ephemeral root reaped");
        assert!(
            global.contains(&fresh_root),
            "fresh ephemeral root survives"
        );
        assert!(
            !mounts.roots().contains(&idle_root),
            "reaped clock entry gone"
        );
        assert!(
            mounts.roots().contains(&fresh_root),
            "fresh clock entry kept"
        );
    }

    // ── Worktree-class idle clock + blocked state (misc 150) ───────────────

    #[test]
    fn worktree_idle_sweep_reaps_quiet_and_activity_refreshes() {
        let tracker = RootTracker::new();
        let mounts = WorktreeMounts::new();
        let idle = PathBuf::from("/wt/idle");
        let fresh = PathBuf::from("/wt/fresh");
        let key_idle = format!("worktree:sess:{}", idle.display());
        let key_fresh = format!("worktree:sess:{}", fresh.display());
        tracker.set_roots(&key_idle, vec![idle.clone()]);
        tracker.set_roots(&key_fresh, vec![fresh.clone()]);

        let t0 = Instant::now();
        mounts.track(&key_idle, &idle, t0);
        mounts.track(&key_fresh, &fresh, t0);

        // Activity under `fresh` refreshes its clock; `idle` is left stale.
        let later = t0 + Duration::from_mins(40);
        mounts.touch_covering(&fresh.join("src/x.rs"), later);

        let expired =
            reap_idle_worktree_roots(&tracker, &mounts, None, later, Duration::from_mins(30));
        assert_eq!(
            expired,
            vec![(key_idle.clone(), idle.clone())],
            "only the quiet worktree root expires",
        );
        let global = tracker.global_roots();
        assert!(!global.contains(&idle), "idle worktree root reaped");
        assert!(global.contains(&fresh), "active worktree root survives");
        assert!(!mounts.contains(&key_idle), "reaped clock entry gone");
        assert!(mounts.contains(&key_fresh), "active clock entry kept");
    }

    #[test]
    fn worktree_blocked_suspends_idle_until_identity_clears() {
        let mounts = WorktreeMounts::new();
        let root = PathBuf::from("/wt/blocked");
        let key = format!("worktree:sess:{}", root.display());
        let t0 = Instant::now();
        mounts.track(&key, &root, t0);

        // Blocked-on-permission → exempt from idle expiry, however long it sits.
        assert!(mounts.mark_blocked(&key), "the root is present and marked");
        assert!(mounts.is_blocked(&key));
        let long_after = t0 + Duration::from_hours(2);
        assert!(
            mounts
                .expired(long_after, Duration::from_mins(30))
                .is_empty(),
            "a blocked root never expires, whatever the idle span",
        );

        // The next identity event clears the flag; the root then expires normally.
        mounts.clear_blocked(&key);
        assert!(!mounts.is_blocked(&key));
        assert_eq!(
            mounts.expired(long_after, Duration::from_mins(30)),
            vec![(key.clone(), root.clone())],
            "once unblocked, a stale root is idle-expired",
        );

        // Qualifying activity also clears the flag (the coarser path).
        mounts.mark_blocked(&key);
        assert!(mounts.is_blocked(&key));
        mounts.touch_covering(&root.join("f.rs"), long_after);
        assert!(
            !mounts.is_blocked(&key),
            "activity under the root clears the blocked flag too",
        );
    }

    #[test]
    fn worktree_blocked_session_marks_all_that_sessions_roots() {
        // The coarse fallback (no agent identity in the permission payload)
        // suspends idle expiry for every worktree root of the session.
        let mounts = WorktreeMounts::new();
        let a = PathBuf::from("/wt/a");
        let b = PathBuf::from("/wt/b");
        let other = PathBuf::from("/wt/other");
        let t0 = Instant::now();
        mounts.track("worktree:sess-1:one", &a, t0);
        mounts.track("worktree:sess-1:two", &b, t0);
        mounts.track("worktree:sess-2:x", &other, t0);

        assert_eq!(mounts.mark_blocked_session("sess-1"), 2);
        assert!(mounts.is_blocked("worktree:sess-1:one"));
        assert!(mounts.is_blocked("worktree:sess-1:two"));
        assert!(
            !mounts.is_blocked("worktree:sess-2:x"),
            "a different session's roots are untouched",
        );
    }

    #[test]
    fn root_is_ephemeral_classifies_by_contributor_prefix() {
        assert!(root_is_ephemeral(&["ephemeral:/p".to_string()]));
        assert!(!root_is_ephemeral(&["hook".to_string()]));
        assert!(
            !root_is_ephemeral(&["ephemeral:/p".to_string(), "hook".to_string()]),
            "any pinned contributor makes the root pinned (an upgraded root)",
        );
        assert!(
            !root_is_ephemeral(&[]),
            "no contributors is never ephemeral"
        );
    }

    #[test]
    fn resolve_touched_paths_joins_relative_and_defaults_to_cwd() {
        let cwd = Path::new("/home/w");
        assert_eq!(
            resolve_touched_paths(
                &[PathBuf::from("src/a.rs"), PathBuf::from("/abs/b.rs")],
                Some(cwd),
            ),
            vec![
                PathBuf::from("/home/w/src/a.rs"),
                PathBuf::from("/abs/b.rs"),
            ],
            "relative args join cwd; absolute args pass through",
        );
        assert_eq!(
            resolve_touched_paths(&[], Some(cwd)),
            vec![PathBuf::from("/home/w")],
            "a bare query's touched target is its cwd",
        );
        assert!(
            resolve_touched_paths(&[], None).is_empty(),
            "no paths and no cwd → nothing to mount",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grep_out_of_root_mounts_ephemeral_root() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, file) = marker_project(&base, "Lattice");

        // No mounted roots — the file is outside every root.
        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // A grep touching the out-of-root file mounts its enclosing project root.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/grep",
                "pattern": "hello",
                "paths": [file.display().to_string()],
            }),
        )
        .await;

        let classes = roots_ls_classes(&ipc_path).await;
        let entry = classes
            .iter()
            .find(|(p, _)| Path::new(p) == project)
            .expect("enclosing project mounted ephemerally on out-of-root grep");
        assert!(entry.1, "the activity-mounted root is classed ephemeral");

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roots_add_upgrades_ephemeral_to_pinned() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, file) = marker_project(&base, "Lattice");

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Mount ephemerally via an out-of-root grep.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/grep",
                "pattern": "hello",
                "paths": [file.display().to_string()],
            }),
        )
        .await;
        assert!(
            roots_ls_classes(&ipc_path)
                .await
                .iter()
                .any(|(p, eph)| Path::new(p) == project && *eph),
            "project starts ephemeral",
        );

        // `pin` on it upgrades it to pinned (and stops expiry).
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "tool/roots-add",
                "path": project.display().to_string(),
            }),
        )
        .await;

        let classes = roots_ls_classes(&ipc_path).await;
        let entry = classes
            .iter()
            .find(|(p, _)| Path::new(p) == project)
            .expect("project still tracked after upgrade");
        assert!(!entry.1, "the upgraded root is now pinned, not ephemeral");

        // The ephemeral contributor was dropped; only `hook` remains.
        let sources = roots_ls(&ipc_path).await;
        let s = sources
            .iter()
            .find(|(p, _)| Path::new(p) == project)
            .expect("project present in sources");
        assert_eq!(
            s.1,
            vec!["hook".to_string()],
            "ephemeral contributor dropped on upgrade, hook remains",
        );

        shutdown.cancel();
    }

    #[test]
    fn remove_root_from_hook_contributor() {
        let tracker = RootTracker::new();
        tracker.add_roots("hook", &[PathBuf::from("/foo"), PathBuf::from("/bar")]);

        assert!(
            tracker.remove_root("hook", Path::new("/foo")),
            "should return true when root was present",
        );
        let global = tracker.global_roots();
        assert_eq!(global.len(), 1);
        assert!(global.contains(&PathBuf::from("/bar")));
        assert!(!global.contains(&PathBuf::from("/foo")));
    }

    #[test]
    fn remove_root_last_entry_removes_contributor() {
        let tracker = RootTracker::new();
        tracker.add_roots("hook", &[PathBuf::from("/only")]);

        assert!(tracker.remove_root("hook", Path::new("/only")));
        assert!(
            tracker.global_roots().is_empty(),
            "global roots should be empty after removing last root",
        );
        // Verify the contributor key is fully removed.
        assert_eq!(
            tracker.refcount(Path::new("/only")),
            0,
            "refcount should be 0",
        );
    }

    #[test]
    fn remove_root_nonexistent_returns_false() {
        let tracker = RootTracker::new();
        tracker.add_roots("hook", &[PathBuf::from("/foo")]);

        assert!(
            !tracker.remove_root("hook", Path::new("/missing")),
            "should return false for missing root",
        );
        assert_eq!(tracker.global_roots().len(), 1);
    }

    #[test]
    fn remove_root_nonexistent_contributor_returns_false() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/foo")]);

        assert!(
            !tracker.remove_root("hook", Path::new("/foo")),
            "should return false for nonexistent contributor",
        );
        assert_eq!(tracker.global_roots().len(), 1);
    }

    #[test]
    fn rm_root_removes_only_hook_roots() {
        let tracker = RootTracker::new();
        // Root is provided by both MCP and hook contributors.
        tracker.set_roots("mcp:10", vec![PathBuf::from("/shared")]);
        tracker.add_roots("hook", &[PathBuf::from("/shared")]);
        assert_eq!(tracker.refcount(Path::new("/shared")), 2);

        // rm-root removes only the hook entry.
        tracker.remove_root("hook", Path::new("/shared"));
        assert_eq!(
            tracker.refcount(Path::new("/shared")),
            1,
            "MCP contributor should still hold the root",
        );
        let global = tracker.global_roots();
        assert!(
            global.contains(&PathBuf::from("/shared")),
            "root should persist (MCP still holds it)",
        );
    }

    #[test]
    fn add_root_hook_contributor() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/existing")]);

        tracker.add_roots("hook", &[PathBuf::from("/new_root")]);

        let global = tracker.global_roots();
        assert_eq!(global.len(), 2);
        assert!(global.contains(&PathBuf::from("/existing")));
        assert!(global.contains(&PathBuf::from("/new_root")));
    }

    #[test]
    fn list_roots_returns_sorted_with_sources() {
        let tracker = RootTracker::new();
        tracker.set_roots("mcp:10", vec![PathBuf::from("/b"), PathBuf::from("/a")]);
        tracker.add_roots("hook", &[PathBuf::from("/a")]);

        let listed = tracker.list_roots();
        assert_eq!(listed.len(), 2);
        // Sorted by path.
        assert_eq!(listed[0].0, PathBuf::from("/a"));
        assert_eq!(listed[0].1, vec!["hook", "mcp:10"]);
        assert_eq!(listed[1].0, PathBuf::from("/b"));
        assert_eq!(listed[1].1, vec!["mcp:10"]);
    }

    #[test]
    fn list_roots_empty_tracker() {
        let tracker = RootTracker::new();
        assert!(tracker.list_roots().is_empty());
    }

    // ── Function-level tests (mutant audit 03-07) ─────────────────

    /// `mcp_socket_path` returns a deterministic path inside `state_dir`.
    #[test]
    fn test_mcp_socket_path_structure() {
        let path = mcp_socket_path();
        assert!(
            path.ends_with("catenary/catenary-mcp.sock"),
            "mcp_socket_path should end with catenary/catenary-mcp.sock, got: {}",
            path.display()
        );
    }

    /// `socket_path` returns a deterministic path inside `state_dir`.
    #[test]
    fn test_socket_path_structure() {
        let path = socket_path();
        assert!(
            path.ends_with("catenary/catenary.sock"),
            "socket_path should end with catenary/catenary.sock, got: {}",
            path.display()
        );
    }

    /// `parse_root_uris` extracts canonical paths from file:// URIs.
    #[test]
    fn test_parse_root_uris_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_path = dir.path().to_path_buf();
        let canonical = root_path.canonicalize().expect("canonicalize");
        let uri = format!("file://{}", root_path.display());

        let roots = vec![crate::mcp::Root {
            uri,
            name: Some("test".to_string()),
        }];
        let result = parse_root_uris(&roots);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], canonical);
    }

    /// `parse_root_uris` skips non-file:// URIs.
    #[test]
    fn test_parse_root_uris_non_file() {
        let roots = vec![crate::mcp::Root {
            uri: "https://example.com".to_string(),
            name: Some("remote".to_string()),
        }];
        let result = parse_root_uris(&roots);
        assert!(result.is_empty(), "non-file URIs should be skipped");
    }

    /// `parse_root_uris` skips paths that fail to canonicalize.
    #[test]
    fn test_parse_root_uris_nonexistent() {
        let roots = vec![crate::mcp::Root {
            uri: "file:///nonexistent/path/that/does/not/exist".to_string(),
            name: None,
        }];
        let result = parse_root_uris(&roots);
        assert!(result.is_empty(), "nonexistent paths should be skipped");
    }

    // ── IPC request/response type tests ──────────────────────────

    /// `GrepRequest` roundtrips through JSON with all fields.
    #[test]
    fn grep_request_roundtrip_full() {
        let req = GrepRequest {
            cwd: Some(PathBuf::from("/home/user/project")),
            pattern: "TODO|FIXME".to_string(),
            paths: vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")],
            exclude: Some("tests/**".to_string()),
            include_gitignored: true,
            include_hidden: false,
            count: false,
            chunked: true,
            flags: GrepFlags::default(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: GrepRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/home/user/project")));
        assert_eq!(parsed.pattern, "TODO|FIXME");
        assert_eq!(
            parsed.paths,
            vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")]
        );
        assert_eq!(parsed.exclude.as_deref(), Some("tests/**"));
        assert!(parsed.include_gitignored);
        assert!(!parsed.include_hidden);
    }

    /// `GrepRequest` deserializes with defaults for optional fields.
    #[test]
    fn grep_request_minimal() {
        let json = r#"{"cwd":"/tmp","pattern":"foo"}"#;
        let req: GrepRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(req.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(req.pattern, "foo");
        assert!(req.exclude.is_none());
        assert!(!req.include_gitignored);
        assert!(!req.include_hidden);
    }

    /// `GrepRequest` skips empty/`None` fields in serialized output.
    #[test]
    fn grep_request_skips_none_fields() {
        let req = GrepRequest {
            cwd: Some(PathBuf::from("/tmp")),
            pattern: "foo".to_string(),
            paths: vec![],
            exclude: None,
            include_gitignored: false,
            include_hidden: false,
            count: false,
            chunked: false,
            flags: GrepFlags::default(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(!json.contains("paths"), "empty paths should be skipped");
        assert!(!json.contains("exclude"), "None exclude should be skipped");
        // Default ripgrep-parity flags are skipped too — a flagless query
        // serializes exactly as before this surface existed.
        assert!(
            !json.contains("ignore_case"),
            "default flags should be skipped"
        );
        assert!(!json.contains("globs"), "empty globs should be skipped");
        assert!(
            !json.contains("chunked"),
            "chunked:false should be skipped — a legacy CLI's wire form is unchanged"
        );
    }

    /// `GrepResponse` roundtrips through JSON.
    #[test]
    fn grep_response_roundtrip() {
        let resp = GrepResponse {
            output: "file.rs:10 matched line".to_string(),
            matches: None,
            files: None,
            skipped: GrepSkips::default(),
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        // An all-searched response omits `skipped` entirely — the wire is byte-
        // for-byte what it was before misc 135.
        assert!(!json.contains("skipped"), "empty skips are omitted: {json}");
        let parsed: GrepResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.output, "file.rs:10 matched line");
        assert!(parsed.matches.is_none());
        assert!(parsed.files.is_none());
        assert!(parsed.skipped.is_empty());
    }

    /// Chunked grep frames (misc 140 phase 2) roundtrip, carry the `"frame"`
    /// version-skew tag, and reject an unrecognized kind rather than misparse.
    #[test]
    fn grep_frame_roundtrips_and_carries_version_tag() {
        let chunk = GrepFrame::Chunk {
            data: "src/a.rs:1:hit\n".to_string(),
        };
        let line = serde_json::to_string(&chunk).expect("serialize chunk");
        assert!(
            line.contains("\"frame\":\"chunk\""),
            "chunk carries the frame tag: {line}"
        );
        assert!(
            matches!(
                serde_json::from_str::<GrepFrame>(&line).expect("parse chunk"),
                GrepFrame::Chunk { data } if data == "src/a.rs:1:hit\n"
            ),
            "chunk roundtrips",
        );

        let end = GrepFrame::End {
            matches: Some(3),
            files: Some(2),
            skipped: GrepSkips::default(),
        };
        let end_line = serde_json::to_string(&end).expect("serialize end");
        assert!(
            end_line.contains("\"frame\":\"end\""),
            "terminator carries the frame tag: {end_line}"
        );

        // The version-skew hinge: a legacy single envelope never carries the
        // frame tag, so the CLI routes it to the single-envelope parse.
        let legacy = GrepResponse {
            output: "x".to_string(),
            matches: None,
            files: None,
            skipped: GrepSkips::default(),
        };
        let legacy_json = serde_json::to_string(&legacy).expect("serialize legacy");
        assert!(
            !legacy_json.contains("\"frame\""),
            "legacy envelope has no frame tag: {legacy_json}"
        );

        // A newer daemon's unknown frame kind fails to parse — honest degradation,
        // never a silent misparse.
        assert!(
            serde_json::from_str::<GrepFrame>(r#"{"frame":"future_kind","data":"x"}"#).is_err(),
            "an unrecognized frame kind is a comprehensible error",
        );
    }

    /// The fairness guard (misc 140 phase 2): the shared search limiter bounds
    /// concurrent walks. Deterministic — asserts permit accounting, never timing.
    #[test]
    fn search_limiter_permits_are_bounded() {
        let limiter = SearchLimiter::new(2);
        assert_eq!(limiter.semaphore.available_permits(), 2);
        let p1 = limiter.semaphore.try_acquire().expect("permit 1");
        let p2 = limiter.semaphore.try_acquire().expect("permit 2");
        assert_eq!(limiter.semaphore.available_permits(), 0);
        assert!(
            limiter.semaphore.try_acquire().is_err(),
            "no third permit while both are held — a burst of searches queues",
        );
        drop(p1);
        assert_eq!(limiter.semaphore.available_permits(), 1);
        assert!(
            limiter.semaphore.try_acquire().is_ok(),
            "a freed permit admits a queued search",
        );
        drop(p2);
    }

    /// The limiter never deadlocks: a zero request clamps to at least one permit.
    #[test]
    fn search_limiter_clamps_to_at_least_one_permit() {
        assert_eq!(SearchLimiter::new(0).semaphore.available_permits(), 1);
    }

    /// `GlobRequest` roundtrips through JSON with all fields.
    #[test]
    fn glob_request_roundtrip_full() {
        let req = GlobRequest {
            cwd: Some(PathBuf::from("/workspace")),
            paths: vec![PathBuf::from("src/"), PathBuf::from("tests/")],
            exclude: Some("target/**".to_string()),
            include_gitignored: false,
            include_hidden: true,
            count: false,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: GlobRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/workspace")));
        assert_eq!(
            parsed.paths,
            vec![PathBuf::from("src/"), PathBuf::from("tests/")]
        );
        assert_eq!(parsed.exclude.as_deref(), Some("target/**"));
        assert!(!parsed.include_gitignored);
        assert!(parsed.include_hidden);
    }

    /// `GlobRequest` deserializes with defaults for optional fields.
    #[test]
    fn glob_request_minimal() {
        let json = r#"{"cwd":"/home","paths":["src/"]}"#;
        let req: GlobRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(req.cwd, Some(PathBuf::from("/home")));
        assert_eq!(req.paths, vec![PathBuf::from("src/")]);
        assert!(req.exclude.is_none());
        assert!(!req.include_gitignored);
        assert!(!req.include_hidden);
    }

    /// `GlobResponse` roundtrips through JSON.
    #[test]
    fn glob_response_roundtrip() {
        let resp = GlobResponse {
            output: "src/\n  main.rs (42 lines)".to_string(),
            paths: None,
            no_match_patterns: vec!["src/**/none.rs".to_string()],
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: GlobResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.output, "src/\n  main.rs (42 lines)");
        assert!(parsed.paths.is_none());
        assert_eq!(parsed.no_match_patterns, vec!["src/**/none.rs".to_string()]);
    }

    /// IPC method constants match expected wire values.
    #[test]
    fn method_constants() {
        assert_eq!(METHOD_GREP, "tool/grep");
        assert_eq!(METHOD_GLOB, "tool/glob");
    }

    // ── resolve_relative tests ──────────────────────────────────

    #[test]
    fn resolve_relative_absolute_unchanged() {
        let result = resolve_relative("/tmp/src/**/*.rs", Path::new("/home/user"));
        assert_eq!(result, "/tmp/src/**/*.rs");
    }

    #[test]
    fn resolve_relative_relative_joined() {
        let result = resolve_relative("src/**/*.rs", Path::new("/home/user/project"));
        assert_eq!(result, "/home/user/project/src/**/*.rs");
    }

    #[test]
    fn resolve_relative_tilde_expanded() {
        let result = resolve_relative("~/src/**/*.rs", Path::new("/home/user/project"));
        // Tilde-expanded paths are absolute → not joined to base.
        assert!(
            !result.starts_with("/home/user/project"),
            "tilde path should not be joined to base"
        );
    }
}
