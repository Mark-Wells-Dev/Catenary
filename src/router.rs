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
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};

use crate::bridge::EditingGuardrail;
use crate::bridge::HookRouter;
use crate::bridge::session::Session;
use crate::bridge::{GlobOutcome, GrepOutcome};
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

/// IPC method string for sed requests.
pub const METHOD_SED: &str = "tool/sed";

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
    /// Page number for paged results (1-based, default: 1).
    #[serde(default = "ipc_default_page")]
    pub page: usize,
    /// Include files ignored by `.gitignore`.
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden files and directories.
    #[serde(default)]
    pub include_hidden: bool,
    /// Return a match/file count instead of rendered results (`--count`).
    #[serde(default)]
    pub count: bool,
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
            "page": self.page,
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
    /// Page number for paged results (1-based, default: 1).
    #[serde(default = "ipc_default_page")]
    pub page: usize,
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
            "page": self.page,
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
}

/// IPC request payload for `catenary sed`.
///
/// Sent as a JSON line over the daemon IPC socket with `"method": "tool/sed"`.
/// [`to_input`](Self::to_input) resolves relative paths and the exclude pattern
/// against `cwd` before dispatching to the substitute engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "orthogonal CLI flags, 1:1 with the clap-parsed sed surface"
)]
pub struct SedRequest {
    /// Working directory from the CLI process (for resolving relative paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Search pattern (`fancy-regex`: the `regex` dialect plus look-around and
    /// back-references).
    pub pattern: String,
    /// Replacement text (`$1` captures; C-escapes interpreted; sed escapes
    /// rejected by the daemon-side validator).
    pub replacement: String,
    /// File/directory paths and glob patterns to scope the edit.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Apply the edit in place; otherwise preview only.
    #[serde(default)]
    pub in_place: bool,
    /// Case-insensitive matching.
    #[serde(default)]
    pub ignore_case: bool,
    /// Case the replacement to match each hit.
    #[serde(default)]
    pub preserve_case: bool,
    /// Replace only the first match per file.
    #[serde(default)]
    pub first: bool,
    /// Glob pattern to exclude from the edit set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
    /// Include files ignored by `.gitignore`.
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden files and directories.
    #[serde(default)]
    pub include_hidden: bool,
    /// Page number for the paged preview (1-based, default: 1).
    #[serde(default = "ipc_default_page")]
    pub page: usize,
}

impl SedRequest {
    /// Resolves relative paths and the exclude pattern against `cwd`, producing
    /// the daemon-side [`crate::bridge::sed::SedInput`] for the substitute
    /// engine.
    ///
    /// Mirrors [`GlobRequest::to_params`]: paths become absolute, an explicit
    /// hidden target auto-enables `include_hidden`, and a basename `exclude`
    /// (no `/`) gets a `**/` prefix for depth-independent matching.
    fn to_input(&self) -> crate::bridge::sed::SedInput {
        let mut include_hidden = self.include_hidden;
        let mut paths = Vec::with_capacity(self.paths.len());
        for p in &self.paths {
            if !p.is_absolute()
                && crate::bridge::session::ResolvedGlob::targets_hidden(&p.to_string_lossy())
            {
                include_hidden = true;
            }
            if p.is_absolute() {
                paths.push(p.clone());
            } else {
                paths.push(
                    self.cwd
                        .as_ref()
                        .map_or_else(|| p.clone(), |cwd| cwd.join(p)),
                );
            }
        }

        let exclude = self.exclude.as_ref().map(|exclude| {
            if exclude.contains('/') {
                self.cwd
                    .as_ref()
                    .map_or_else(|| exclude.clone(), |cwd| resolve_relative(exclude, cwd))
            } else {
                format!("**/{exclude}")
            }
        });

        crate::bridge::sed::SedInput {
            pattern: self.pattern.clone(),
            replacement: self.replacement.clone(),
            paths,
            in_place: self.in_place,
            ignore_case: self.ignore_case,
            preserve_case: self.preserve_case,
            first: self.first,
            exclude,
            include_gitignored: self.include_gitignored,
            include_hidden,
            page: self.page,
        }
    }
}

/// IPC response for `catenary sed`.
///
/// Returned as a single JSON line over the daemon IPC socket.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SedResponse {
    /// Rendered preview / write summary.
    #[serde(default)]
    pub output: String,
}

/// Default page number for IPC tool requests (1-based).
const fn ipc_default_page() -> usize {
    1
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
    /// Host CLI name from the hook `format` field (`claude`/`gemini`/…).
    client_name: Option<String>,
    /// When the session first connected (ISO 8601).
    started_at: String,
    /// Workspace roots from the session's own payload (`cwd` /
    /// `workspacePaths`) — never correlated to MCP roots.
    roots: Vec<String>,
}

/// Extracts a session's workspace roots from its hook payload.
///
/// Host-agnostic: Antigravity sends `workspacePaths` (array), Claude Code and
/// Gemini CLI send `cwd` (string). Returns an empty vec when neither is
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
            })
            .collect()
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
    /// Per-key hook→CLI handoff (ADR 014). Replaces the single global slot +
    /// 1-permit semaphore: each [`HandoffKey`] serializes independently, so a
    /// `diagnostics` handoff never stalls a `sed` handoff — or any other
    /// session — daemon-wide.
    handoff: KeyedHandoff,
}

/// A staged hook→CLI handoff, deposited under a [`HandoffKey`] by the
/// `PreToolUse` hook and consumed by the matching CLI command.
///
/// The payload direction differs by key (see [`HandoffPayload`]): `diagnostics`
/// is *data-back* (the drained file set), `sed` is *identity-forward* (the
/// session identity the daemon keys the runtime-changed set under).
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

/// The direction-specific payload of a staged [`HandoffContext`].
enum HandoffPayload {
    /// `diagnostics` — *data-back*: the hook drains the accumulated set and the
    /// `catenary diagnostics` CLI command retrieves it.
    Diagnostics {
        /// Accumulated files from the editing session.
        files: Vec<PathBuf>,
        /// Number of files skipped because they were outside tracked workspace
        /// roots (no LSP coverage).
        filtered: usize,
        /// Host session id (from the staging hook). The bare `catenary
        /// diagnostics` process is identity-less, so the session id rides the
        /// handoff — the daemon names the per-session overflow file with it.
        session_id: String,
    },
    /// `sed` — *identity-forward*: the hook (the only holder of identity for
    /// this tool-use) stages it; the `catenary sed --in-place` process connects,
    /// performs the write, and the daemon accumulates the runtime-changed set
    /// under this identity.
    SedIdentity {
        /// Session ID from the host payload (`None` for hooks without one).
        session_id: Option<String>,
        /// Agent ID from the host payload.
        agent_id: String,
    },
}

/// Correlation key for the hook→CLI handoff — the catenary subcommand alone
/// (ADR 014).
///
/// `cwd`, pattern, and path are recorded for observability bucketing but are
/// *not* key material. Only the two load-bearing, bare-only commands stage a
/// handoff; stateless `grep`/`glob` self-scope with a daemon-minted UUID and
/// never correlate here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum HandoffKey {
    /// `catenary diagnostics` — data-back: the hook stages the accumulated
    /// file set, the CLI drains it.
    Diagnostics,
    /// `catenary sed` — identity-forward: the hook stages session identity,
    /// the sed process reports its runtime-changed set. Wired by ticket 08;
    /// the key and its semaphore exist now so 08 plugs into the final
    /// mechanism rather than rebuilding it.
    Sed,
}

impl HandoffKey {
    /// Every handoff key — used to eagerly create the per-key semaphores.
    /// Cardinality is ≤ 2 by design (ADR 014).
    const ALL: [Self; 2] = [Self::Diagnostics, Self::Sed];
}

/// Per-key handoff self-heal timeout.
///
/// Clears a staged handoff the CLI never consumes — e.g. the host killed the
/// `catenary diagnostics` / `catenary sed` subprocess between `PreToolUse` and
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
/// overwrite, no double-consume) — and its own slot + timeout — **per-key
/// independence** (a `diagnostics` handoff and a `sed` handoff proceed
/// concurrently, and neither can stall the daemon as the old global lock
/// could). Cardinality ≤ 2.
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
#[cfg(unix)]
#[derive(Clone)]
struct RootTracker {
    /// Per-contributor root sets. The global root set is the union of
    /// all values.
    contributors: Arc<std::sync::Mutex<HashMap<String, HashSet<PathBuf>>>>,
}

#[cfg(unix)]
impl RootTracker {
    fn new() -> Self {
        Self {
            contributors: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Replaces a contributor's root set.
    fn set_roots(&self, contributor: &str, roots: Vec<PathBuf>) {
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(contributor.to_string(), roots.into_iter().collect());
    }

    /// Adds roots to a contributor's set (does not remove existing ones).
    fn add_roots(&self, contributor: &str, roots: &[PathBuf]) {
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(contributor.to_string())
            .or_default()
            .extend(roots.iter().cloned());
    }

    /// Removes a contributor entirely.
    fn remove_contributor(&self, contributor: &str) {
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(contributor);
    }

    /// Removes a single root from a contributor's set.
    ///
    /// Returns `true` if the root was present and removed, `false` if
    /// the contributor or root was not found.
    #[allow(
        clippy::option_if_let_else,
        reason = "map_or causes double-borrow on the Mutex guard"
    )]
    fn remove_root(&self, contributor: &str, root: &Path) -> bool {
        let mut map = self
            .contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(roots) = map.get_mut(contributor) {
            let removed = roots.remove(root);
            if roots.is_empty() {
                map.remove(contributor);
            }
            removed
        } else {
            false
        }
    }

    /// Returns the union of all contributors' root sets.
    fn global_roots(&self) -> Vec<PathBuf> {
        let map = self
            .contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut all = HashSet::new();
        for roots in map.values() {
            all.extend(roots.iter().cloned());
        }
        drop(map);
        all.into_iter().collect()
    }

    /// Returns all roots with their contributor sources.
    ///
    /// Each entry is `(path, sources)` where `sources` is a sorted list
    /// of contributor keys (e.g., `["hook", "mcp:3"]`).
    fn list_roots(&self) -> Vec<(PathBuf, Vec<String>)> {
        let map = self
            .contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Invert: root → list of contributors.
        let mut root_sources: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for (contributor, roots) in &*map {
            for root in roots {
                root_sources
                    .entry(root.clone())
                    .or_default()
                    .push(contributor.clone());
            }
        }
        drop(map);

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
        self.contributors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|roots| roots.contains(root))
            .count()
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
                    if let Some(ctx) = &self.hook_ctx {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_hook_dispatch(stream, ctx, shutdown).await {
                                debug!(
                                    source = Source::DaemonDispatch.as_str(),
                                    "hook connection error: {e}",
                                );
                            }
                        });
                    } else {
                        tokio::spawn(async move {
                            if let Err(e) = handle_hook_connection(stream, shutdown).await {
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
                                let paths = parse_root_uris(&roots);
                                tracker.set_roots(&mcp_key, paths);
                                let global = tracker.global_roots();
                                tokio::runtime::Handle::current()
                                    .block_on(session.sync_roots(global))?;
                                Ok(())
                            }));
                        }
                        (None, Some(cm), _) => {
                            mcp = mcp.on_roots_changed(Box::new(move |roots| {
                                let paths = parse_root_uris(&roots);
                                tokio::runtime::Handle::current().block_on(cm.sync_roots(paths))?;
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
                    let global = tracker.global_roots();
                    let sync_result = if let Some(ref session) = session_cleanup {
                        session.sync_roots(global).await
                    } else if let Some(ref cm) = lsp_cleanup {
                        cm.sync_roots(global).await
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
        // Wire the live session board onto the daemon snapshot so `state.json`
        // carries the rich session board (observability ticket 05). The writer
        // pulls this at each flush; `None` outside daemon mode.
        if let Some(snapshot) = &session.snapshot {
            snapshot.set_session_board(Arc::new(SessionBoardImpl {
                sessions: sessions.clone(),
            }));
        }

        self.hook_ctx = Some(HookDispatchContext {
            sessions,
            primary: session,
            _logging: self.logging.clone(),
            root_tracker: Some(root_tracker),
            editing_guardrail: Arc::new(EditingGuardrail::new()),
            handoff: KeyedHandoff::new(),
        });
        self
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

/// Handles a single hook connection.
///
/// Reads the JSON request, logs the method for visibility, and sends an
/// empty response (which means "allow" in the hook protocol). Recognizes
/// the `"tool/shutdown"` method from `catenary stop` and cancels the daemon
/// shutdown token. Used when no shared session is configured (test mode).
#[cfg(unix)]
async fn handle_hook_connection(
    stream: tokio::net::UnixStream,
    shutdown: CancellationToken,
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
            info!(
                source = Source::DaemonLifecycle.as_str(),
                "shutdown requested via stop command",
            );
            writer.write_all(b"{\"status\":\"ok\"}\n").await?;
            writer.shutdown().await?;
            shutdown.cancel();
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
/// [`Session::new_for_daemon`]) with independent editing state and
/// notification queue. The `HookRouter` wraps the per-session `Session`
/// with its own turn counter and debounce state.
///
/// Registers the session with the shared [`crate::logging::notification_router::NotificationRouter`]
/// so `warn!()` / `error!()` events carrying this `session_id` in
/// their span context route to this session's notification queue.
///
/// Populates the session's board metadata on first creation; the daemon
/// snapshot (`state.json`) surfaces it to the TUI dashboard.
#[cfg(unix)]
fn get_or_create_router(
    ctx: &HookDispatchContext,
    session_id: &str,
    raw: &serde_json::Value,
) -> Arc<HookRouter> {
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

            // Register session with the notification router so
            // events carrying this session_id route to its queue.
            session.notification_router.register_session(session_id);

            // Board metadata from this session's own hook payload (ticket 05):
            // the `format` field is the host label (client_name), the connect
            // time, and the session's own workspace roots. The daemon snapshot
            // (`state.json`) surfaces this to the TUI dashboard.
            let client_name = raw.get("format").and_then(|v| v.as_str());
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

    // Bump `last_seen` on EVERY dispatch, not only on create. Every agent tool
    // call funnels through here — including the Bash hooks that wrap
    // `catenary grep`/`glob`/`diagnostics`/`sed` (only those commands' own
    // subprocess IPC bypasses the session) — so `last_seen` is the one uniform
    // liveness signal a hook session has, far richer than `last_action`, which
    // moves only on edit / diagnostics / sed. The bump takes the session's own
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
/// to the note alone, matching the prior behavior. Returns `output`
/// unchanged when nothing was filtered.
#[cfg(unix)]
fn with_out_of_roots_note(output: String, filtered: usize) -> String {
    if filtered == 0 {
        return output;
    }
    let note = format!(
        "({filtered} edit{} outside tracked roots \u{2014} not checked; see `catenary roots -h`)",
        if filtered == 1 { "" } else { "s" },
    );
    if output.is_empty() {
        note
    } else {
        format!("{output}\n{note}")
    }
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

    // Handle shutdown from `catenary stop`.
    if method == "tool/shutdown" {
        info!(
            source = Source::DaemonLifecycle.as_str(),
            "shutdown requested via stop command",
        );
        writer.write_all(b"{\"status\":\"ok\"}\n").await?;
        writer.shutdown().await?;
        shutdown.cancel();
        return Ok(());
    }

    // ── List tracked roots ─────────────────────────────────────
    //
    // `tool/roots-ls` is sent by `catenary roots ls`. Returns all
    // tracked workspace roots with their contributor sources.
    if method == "tool/roots-ls" {
        let roots = ctx
            .root_tracker
            .as_ref()
            .map_or_else(Vec::new, RootTracker::list_roots);

        let roots_json: Vec<serde_json::Value> = roots
            .into_iter()
            .map(|(path, sources)| {
                serde_json::json!({
                    "path": path.display().to_string(),
                    "sources": sources,
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

    // Extract session_id for routing. Falls back to "default" for hooks
    // that don't carry a session_id (backward compatibility).
    let session_id = raw
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    // ── Session-end cleanup ───────────────────────────────────────
    //
    // Fires when the host CLI sends a SessionEnd hook (exit, /clear,
    // resume, logout). Cleans up session-scoped state: editing
    // guardrail, notification router, session registry, and roots.
    //
    // Short-circuits before get_or_create_router to avoid creating
    // a new session just to immediately clean it up.
    if method == "session-end/cleanup" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        // Release editing guardrail locks (idempotent if MCP
        // disconnect already ran).
        ctx.editing_guardrail.release_all(&session_id);

        // Remove session from notification router (idempotent if
        // MCP disconnect already ran).
        ctx.primary.notification_router.remove_session(&session_id);

        // Remove the session from the registry.
        ctx.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session_id);

        // Best-effort removal from the board — mark the snapshot dirty so the
        // next flush drops it. Not a tombstone: a Claude resume re-creates the
        // entry via `get_or_create_router`, and Antigravity sends no
        // `session-end` at all. `last_seen` is the authoritative liveness signal
        // (ticket 05a).
        ctx.primary.touch_snapshot();

        // Best-effort removal of this session's diagnostics overflow file. The
        // authoritative GC is the daemon-startup sweep (no teardown signal is
        // reliable — Antigravity has no session-end), but a graceful end lets
        // us reclaim the runtime-dir file immediately.
        crate::bridge::overflow::remove_diagnostics(&crate::paths::runtime_dir(), &session_id);

        if let Some(ref tracker) = ctx.root_tracker {
            // Sync the reduced root set.
            let global = tracker.global_roots();
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

        // Race grep execution against client disconnect so a killed
        // CLI process doesn't leave the pipeline running indefinitely.
        let cancel_on_disconnect = cancel.clone();
        let response = tokio::select! {
            result = ctx.primary.grep.execute(&params, Some(&parent_id), &cancel).instrument(span.clone()) => {
                match result {
                    Ok(GrepOutcome::Rendered(output)) => GrepResponse {
                        output,
                        matches: None,
                        files: None,
                    },
                    Ok(GrepOutcome::Count { matches, files }) => GrepResponse {
                        output: String::new(),
                        matches: Some(matches),
                        files: Some(files),
                    },
                    Err(e) => GrepResponse {
                        output: format!("grep error: {e}"),
                        matches: None,
                        files: None,
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

        // Race glob execution against client disconnect so a killed
        // CLI process doesn't leave the pipeline running indefinitely.
        let cancel_on_disconnect = cancel.clone();
        let response = tokio::select! {
            result = ctx.primary.glob.execute(&params, Some(&parent_id), &cancel).instrument(span.clone()) => {
                match result {
                    Ok(GlobOutcome::Rendered(output)) => GlobResponse { output, paths: None },
                    Ok(GlobOutcome::Count { paths }) => GlobResponse {
                        output: String::new(),
                        paths: Some(paths),
                    },
                    Err(e) => GlobResponse {
                        output: format!("glob error: {e}"),
                        paths: None,
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
    // unchanged). Acquires the handoff lock, drains files, releases the
    // editing guardrail, and deposits the file list for the subsequent
    // CLI command.
    if method == "pre-tool/editing-stop" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        let router = get_or_create_router(&ctx, &session_id, &raw);

        // Acquire the `diagnostics` handoff permit. Blocks only behind another
        // in-flight *diagnostics* handoff (per-key, ADR 014) — never daemon-wide
        // — and holds for milliseconds at most.
        let permit = ctx.handoff.acquire(HandoffKey::Diagnostics).await?;

        // Drain accumulated files from EditingManager.
        let (files, filtered) = router.session.editing.drain_all_and_clear();

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            file_count = files.len(),
            filtered,
            "diagnostics: drained files from EditingManager",
        );

        // Release the editing guardrail.
        ctx.editing_guardrail.release_all(&session_id);

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
                    session_id: session_id.clone(),
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
        // Take the file list and parent_id from the `diagnostics` slot,
        // releasing the permit immediately. The permit must not be held during
        // the diagnostics pipeline (which may take seconds). Consuming the
        // HandoffContext drops it, releasing the owned semaphore permit.
        let handoff = ctx.handoff.consume(HandoffKey::Diagnostics).and_then(|h| {
            match h.payload {
                HandoffPayload::Diagnostics {
                    files,
                    filtered,
                    session_id,
                } => Some((files, filtered, session_id, h.parent_id)),
                // The diagnostics key only ever carries a diagnostics payload.
                HandoffPayload::SedIdentity { .. } => None,
            }
        });

        // Extract scope_id early so we can emit the incoming hook
        // event before running the diagnostics pipeline. This ensures
        // the tool/editing-stop event is the first message in the
        // parent_id group, making it the scope header in the TUI
        // (matching the grep/glob pattern).
        let scope_id = match &handoff {
            Some((_, _, _, parent_id)) => parent_id.clone(),
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

        // `dirty` drives the CLI's clean/dirty exit code (ticket 11). Faults
        // (no daemon, IPC/parse failure) are detected CLI-side and exit 2; the
        // daemon only ever reports clean or dirty here.
        let (dirty, output) = if let Some((files, filtered, session_id, _)) = handoff {
            if files.is_empty() {
                // Nothing covered to diagnose — the note (if any) stands alone.
                (false, with_out_of_roots_note(String::new(), filtered))
            } else {
                // Reflect the run on the session board: status → diagnostics
                // for its duration (the editing accumulator already drained at
                // the prepare step), then record the result as last_action
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
                        .process_files_batched(&files, Some(&scope_id), &session_id) => outcome,
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
                // Surface any filtered edits alongside the covered-file results,
                // so a mixed batch never silently hides the unchecked files.
                (
                    outcome.dirty,
                    with_out_of_roots_note(outcome.output, filtered),
                )
            }
        } else {
            // Handoff slot was empty — timeout expired or double-consume.
            (
                false,
                "diagnostics handoff expired — no files available".to_string(),
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

        // Structured response so the CLI can map status → exit code while still
        // printing the diagnostics text. Mirrors the grep/glob JSON envelope.
        let envelope = serde_json::json!({
            "status": if dirty { "dirty" } else { "clean" },
            "output": output,
        });
        let mut payload = serde_json::to_vec(&envelope)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Sed handoff: prepare (identity-forward) ──────────────────
    //
    // `pre-tool/sed` is sent by the PreToolUse hook when the agent runs
    // `catenary sed --in-place`. The hook is the only holder of
    // `(session_id, agent_id)` for this tool-use, but the changed set is a
    // runtime result the daemon computes — so the hook stages the *identity*
    // and the sed process reports its changed set back (the inverse of the
    // diagnostics data-back handoff).
    if method == "pre-tool/sed" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        // Ensure the session exists for TUI discovery and host_payload cwd.
        let _ = get_or_create_router(&ctx, &session_id, &raw);

        let agent_id = raw
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Acquire the `sed` handoff permit. Per-key (ADR 014): blocks only behind
        // another in-flight *sed* handoff, never daemon-wide, for milliseconds.
        let permit = ctx.handoff.acquire(HandoffKey::Sed).await?;
        let handoff_parent_id = uuid::Uuid::new_v4().to_string();

        ctx.handoff.stage(
            HandoffKey::Sed,
            HandoffContext {
                parent_id: handoff_parent_id,
                payload: HandoffPayload::SedIdentity {
                    session_id: Some(session_id.clone()),
                    agent_id,
                },
                permit,
            },
        );

        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            "sed identity handoff staged",
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

    // ── Sed: run ─────────────────────────────────────────────────
    //
    // `tool/sed` is sent by the `catenary sed` CLI command. For `--in-place` the
    // staged identity is consumed *up front* — the per-session `Session` both
    // guards writes (the cross-session per-root editing guardrail, exactly as
    // Edit/Write do) and, after the run, accumulates the changed set through the
    // same `has_lsp_coverage` gate. The substitute engine itself runs on a
    // blocking thread so a broad sweep can't stall the daemon.
    if method == METHOD_SED {
        let sed_req: SedRequest =
            serde_json::from_value(raw.clone()).map_err(|e| anyhow!("invalid sed request: {e}"))?;
        let in_place = sed_req.in_place;
        let input = sed_req.to_input();
        let parent_id = uuid::Uuid::new_v4().to_string();

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&parent_id),
            &raw.to_string(),
            "incoming hook",
        );

        let budget = ctx
            .primary
            .config
            .tools
            .as_ref()
            .map_or(4000, |t| t.grep.budget as usize);

        // Consume the staged identity before the write so the session can both
        // guard and accumulate. `None` ⇒ preview, or an expired/absent handoff
        // (writes proceed unguarded and untracked — the same degradation as an
        // expired diagnostics handoff).
        let identity = if in_place {
            ctx.handoff
                .consume(HandoffKey::Sed)
                .and_then(|h| match h.payload {
                    HandoffPayload::SedIdentity {
                        session_id,
                        agent_id,
                    } => Some((session_id, agent_id)),
                    HandoffPayload::Diagnostics { .. } => None,
                })
        } else {
            None
        };
        let router = identity
            .as_ref()
            .map(|(sid, _)| get_or_create_router(&ctx, sid.as_deref().unwrap_or("default"), &raw));
        let guard_session = router.as_ref().map(|r| r.session.clone());

        // The bare preview streams its full diff to a per-invocation overflow
        // file (ticket 11a) when its in-memory render caps truncate. Stateless
        // (grep-class): a daemon-minted UUID names `sed-<uuid>.txt`, swept at
        // startup + bounded by an in-lifetime cap. `--in-place` has no preview,
        // so it carries no overflow context.
        let overflow = (!in_place).then(|| crate::bridge::sed::PreviewOverflow {
            base: crate::paths::runtime_dir(),
            id: uuid::Uuid::new_v4().to_string(),
        });

        let outcome = match tokio::task::spawn_blocking(move || {
            // Per-file write guard: deny files whose root another session holds.
            // Rootless files (single-file coverage) carry no guardrail.
            let guard = |path: &Path| -> bool {
                guard_session.as_ref().is_none_or(|session| {
                    session.resolve_root(path).is_none_or(|root| {
                        session
                            .editing_guardrail
                            .as_ref()
                            .is_none_or(|g| g.try_acquire(&root, &session.instance_id).is_ok())
                    })
                })
            };
            crate::bridge::sed::execute_with_overflow(&input, budget, guard, overflow)
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => crate::bridge::sed::SedOutcome {
                output: format!("sed error: {e}"),
                changed: Vec::new(),
            },
        };

        // Bug #23: the daemon performed the write, so refresh the shared
        // SymbolIndex (and enrichment cache) for the changed files — grep/glob
        // enrichment would otherwise serve pre-rename enclosing-symbol labels
        // and ranges until a later access finds an empty table. Unconditional
        // (independent of the editing handoff identity); empty on preview, where
        // it is a no-op.
        ctx.primary.invalidate_symbols(&outcome.changed);

        // Identity-forward accumulation: route the LSP-covered changed files
        // into the editing set, exactly like an Edit/Write would.
        if let Some((sid, agent_id)) = identity
            && let Some(router) = router.as_ref()
            && !outcome.changed.is_empty()
        {
            let mut started = false;
            let mut accumulated = 0usize;
            for file in &outcome.changed {
                if router.session.has_lsp_coverage(file) {
                    if !started {
                        let _ = router
                            .session
                            .editing
                            .start_editing(sid.as_deref(), &agent_id);
                        started = true;
                    }
                    router
                        .session
                        .editing
                        .add_file(sid.as_deref(), &agent_id, file.clone());
                    accumulated += 1;
                } else {
                    router.session.editing.increment_filtered();
                }
            }
            debug!(
                source = Source::DaemonDispatch.as_str(),
                changed = outcome.changed.len(),
                accumulated,
                "sed: accumulated changed files for diagnostics",
            );

            // Surface the sed write on the snapshot session board (ticket 05).
            let summary = if outcome.changed.len() == 1 {
                format!("sed {}", router.session.display_path(&outcome.changed[0]))
            } else {
                format!("sed {} files", outcome.changed.len())
            };
            router.session.set_last_action(summary);
        }

        let response = SedResponse {
            output: outcome.output,
        };
        let mut payload = serde_json::to_vec(&response)?;

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&parent_id),
            std::str::from_utf8(&payload).unwrap_or_default(),
            "outgoing hook response",
        );

        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.shutdown().await?;
        return Ok(());
    }

    // ── Root management ──────────────────────────────────────────
    //
    // `tool/roots-add` and `tool/roots-rm` are sent by the CLI commands
    // (`catenary roots add`, `catenary roots rm`). The PreToolUse hook
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
                let global = tracker.global_roots();
                if let Err(e) = ctx.primary.sync_roots(global).await {
                    debug!(
                        source = Source::DaemonDispatch.as_str(),
                        "root sync after add-root failed: {e}",
                    );
                }
                info!(
                    source = Source::DaemonDispatch.as_str(),
                    path = %canonical.display(),
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
                    let global = tracker.global_roots();
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

    let result = router.dispatch(request);

    let envelope = HookResponseEnvelope {
        result: result.result,
        system_message: result.system_message,
    };
    let response = if envelope.result.is_some() || envelope.system_message.is_some() {
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
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default(),
            roots,
            logging.clone(),
            instance_id,
            runtime,
            notification_router,
            None,
        ));

        SessionManager::bind_at(&mcp_socket_in(dir), &ipc_socket_in(dir), logging)
            .expect("bind")
            .with_session(session)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_board_builds_rich_entries() {
        use crate::state_snapshot::{SessionBoard, SessionStatus};

        let instance_id: Arc<str> = "sess-1".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default(),
            vec![],
            LoggingServer::new(),
            instance_id.clone(),
            tokio::runtime::Handle::current(),
            notification_router,
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
        let board = SessionBoardImpl { sessions };

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

        // An active editing accumulator → status `editing`.
        session
            .editing
            .start_editing(Some("sess-1"), "")
            .expect("start editing");
        assert_eq!(board.sessions()[0].status, SessionStatus::Editing);

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
            "method": "pre-agent/turn-start",
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
            "method": "pre-agent/turn-start",
            "session_id": "session-a"
        });
        let req_b = serde_json::json!({
            "method": "pre-agent/turn-start",
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
            router_a.session.editing.add_file(
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
    async fn session_state_turn_counter_per_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // Send two turn-start hooks to session A.
        let req_a = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "session-a"
        });
        let _ = hook_roundtrip(&ipc_path, &req_a).await;
        let _ = hook_roundtrip(&ipc_path, &req_a).await;

        // Send one turn-start hook to session B.
        let req_b = serde_json::json!({
            "method": "pre-agent/turn-start",
            "session_id": "session-b"
        });
        let _ = hook_roundtrip(&ipc_path, &req_b).await;

        // Verify each session has its own turn counter by checking
        // that session A and B exist independently.
        assert_eq!(manager.session_count(), 2);

        // Verify independence through the hook_ctx.
        let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
        let sessions = ctx.sessions.lock().expect("lock");
        let router_a = Arc::clone(&sessions.get("session-a").expect("session-a").router);
        let router_b = Arc::clone(&sessions.get("session-b").expect("session-b").router);
        drop(sessions);
        assert_eq!(router_a.turn(), 2, "session A should have turn 2");
        assert_eq!(router_b.turn(), 1, "session B should have turn 1");

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
        // Nothing filtered → output is untouched.
        assert_eq!(
            with_out_of_roots_note("src/main.rs\n\t[clean]".to_string(), 0),
            "src/main.rs\n\t[clean]",
        );

        // Mixed batch: the covered-file results are preserved and the note
        // is appended so the unchecked edits are not silently hidden.
        let mixed = with_out_of_roots_note("src/main.rs\n\t[clean]".to_string(), 2);
        assert!(
            mixed.starts_with("src/main.rs\n\t[clean]\n"),
            "got: {mixed}"
        );
        assert!(
            mixed.contains("2 edits outside tracked roots"),
            "got: {mixed}"
        );
        assert!(mixed.contains("not checked"), "got: {mixed}");

        // All-uncovered batch: the note stands alone, no stray leading newline.
        let alone = with_out_of_roots_note(String::new(), 1);
        assert_eq!(
            alone,
            "(1 edit outside tracked roots \u{2014} not checked; see `catenary roots -h`)",
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

        // Prepare handoff — should drain the accumulated file.
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
        // With no LSP servers, process_files_batched returns "[clean]"
        // for files without coverage. The response should not be the
        // expired message.
        assert!(
            !response.contains("handoff expired"),
            "handoff should not be expired, got: {response}",
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

    /// `catenary sed --in-place` writes the file and the daemon accumulates the
    /// runtime-changed set under the hook-staged `(session_id, agent_id)`
    /// (the identity-forward handoff, ADR 014 / ticket 08).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sed_identity_handoff() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        // Root the session at the tempdir so the edited file has LSP coverage
        // (tier 1–2) and is accumulated rather than filtered.
        let manager = Arc::new(bind_with_session_roots(
            dir.path(),
            vec![dir.path().to_path_buf()],
        ));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let file = dir.path().join("rename_me.rs");
        std::fs::write(&file, "let omni = 1;\n").expect("write file");

        // The PreToolUse hook stages the identity forward.
        let stage = serde_json::json!({
            "method": "pre-tool/sed",
            "agent_id": "",
            "session_id": "sess-1",
        });
        let line = hook_roundtrip(&ipc_path, &stage).await;
        assert!(line.contains("ok"), "stage should succeed, got: {line}");

        // The sed process connects, writes, and reports its changed set.
        let run = serde_json::json!({
            "method": "tool/sed",
            "pattern": "omni",
            "replacement": "lattice",
            "paths": [file.to_string_lossy()],
            "in_place": true,
        });
        let response = hook_roundtrip(&ipc_path, &run).await;
        let parsed: SedResponse =
            serde_json::from_str(response.trim()).expect("parse sed response");
        assert!(
            parsed.output.contains("replacements"),
            "in-place run reports replacements, got: {}",
            parsed.output
        );

        // The write landed.
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "let lattice = 1;\n"
        );

        // The changed file is accumulated under the staged (session_id, agent_id).
        let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
        let sessions = ctx.sessions.lock().expect("lock sessions");
        let router = Arc::clone(&sessions.get("sess-1").expect("session sess-1").router);
        drop(sessions);
        let tracked = router.session.editing.files(Some("sess-1"), "");
        assert_eq!(
            tracked,
            vec![file],
            "the changed file is keyed under the staged identity",
        );

        shutdown.cancel();
    }

    /// Bug #23: a `catenary sed --in-place` write invalidates the shared
    /// `SymbolIndex` for the changed files, so `grep`/`glob` enrichment
    /// re-indexes fresh instead of serving pre-rename enclosing-symbol labels.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sed_in_place_invalidates_symbol_index() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_with_session_roots(
            dir.path(),
            vec![dir.path().to_path_buf()],
        ));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let file = dir.path().join("rename_me.rs");
        std::fs::write(&file, "let omni = 1;\n").expect("write file");

        // Pre-seed the shared symbol index with (synthetic) pre-rename symbols,
        // as an earlier grep/glob/diagnostics access would have.
        let index = {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            Arc::clone(ctx.primary.symbol_index.as_ref().expect("symbol index"))
        };
        let symbols = serde_json::json!([{
            "name": "omni",
            "kind": 13,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 13 } },
            "selectionRange": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 8 } }
        }]);
        {
            let idx = index.lock().expect("lock symbol index");
            idx.populate_from_document_symbols(&file, &symbols)
                .expect("populate");
            assert!(
                idx.has_symbols_for(&file),
                "symbols are present before the sed write",
            );
        }

        // Stage identity + run the in-place rename.
        let stage = serde_json::json!({
            "method": "pre-tool/sed",
            "agent_id": "",
            "session_id": "sess-1",
        });
        let line = hook_roundtrip(&ipc_path, &stage).await;
        assert!(line.contains("ok"), "stage should succeed, got: {line}");

        let run = serde_json::json!({
            "method": "tool/sed",
            "pattern": "omni",
            "replacement": "lattice",
            "paths": [file.to_string_lossy()],
            "in_place": true,
        });
        let response = hook_roundtrip(&ipc_path, &run).await;
        let parsed: SedResponse =
            serde_json::from_str(response.trim()).expect("parse sed response");
        assert!(
            parsed.output.contains("replacements"),
            "in-place run reports replacements, got: {}",
            parsed.output
        );
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "let lattice = 1;\n",
        );

        // The stale rows were dropped — the next enrichment access re-indexes.
        {
            let idx = index.lock().expect("lock symbol index");
            assert!(
                idx.needs_population(&file),
                "sed --in-place invalidated the stale symbol rows",
            );
        }

        shutdown.cancel();
    }

    // ── Keyed handoff structure tests (ADR 014) ───────────────────────

    /// Different keys are independent: holding the `diagnostics` permit must
    /// not block a `sed` acquire (the `timeout` would only elapse if it did).
    #[tokio::test]
    async fn keyed_handoff_keys_are_independent() {
        let handoff = KeyedHandoff::new();

        // Hold the diagnostics permit for the whole test.
        let _diag = handoff
            .acquire(HandoffKey::Diagnostics)
            .await
            .expect("acquire diagnostics");

        // A sed acquire proceeds immediately — its own permit, independent key.
        let sed =
            tokio::time::timeout(Duration::from_secs(1), handoff.acquire(HandoffKey::Sed)).await;
        assert!(
            sed.is_ok(),
            "sed handoff must not block on a held diagnostics permit",
        );
    }

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
                    session_id: "sess-1".to_string(),
                },
                permit,
            },
        );

        let consumed = handoff
            .consume(HandoffKey::Diagnostics)
            .expect("consume staged context");
        assert_eq!(consumed.parent_id, "scope-1");
        if let HandoffPayload::Diagnostics {
            files,
            filtered,
            session_id,
        } = &consumed.payload
        {
            assert_eq!(files, &vec![PathBuf::from("/tmp/a.rs")]);
            assert_eq!(*filtered, 2);
            assert_eq!(session_id, "sess-1");
        } else {
            unreachable!("expected diagnostics payload");
        }

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
    /// permit — and only its own key is affected.
    ///
    /// Injects a short self-heal timeout via [`KeyedHandoff::with_timeout`] so
    /// the clear-on-timeout path runs fast, independent of the production
    /// [`HANDOFF_TIMEOUT`].
    #[tokio::test]
    async fn keyed_handoff_timeout_clears_only_its_key() {
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
                    session_id: "x".to_string(),
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

        // The untouched `sed` key is unaffected by the diagnostics timeout.
        let _sed = handoff
            .acquire(HandoffKey::Sed)
            .await
            .expect("sed key independent of diagnostics timeout");
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
            page: 2,
            include_gitignored: true,
            include_hidden: false,
            count: false,
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
        assert_eq!(parsed.page, 2);
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
        assert_eq!(req.page, 1);
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
            page: 1,
            include_gitignored: false,
            include_hidden: false,
            count: false,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(!json.contains("paths"), "empty paths should be skipped");
        assert!(!json.contains("exclude"), "None exclude should be skipped");
    }

    /// `GrepResponse` roundtrips through JSON.
    #[test]
    fn grep_response_roundtrip() {
        let resp = GrepResponse {
            output: "file.rs:10 matched line".to_string(),
            matches: None,
            files: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: GrepResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.output, "file.rs:10 matched line");
        assert!(parsed.matches.is_none());
        assert!(parsed.files.is_none());
    }

    /// `GlobRequest` roundtrips through JSON with all fields.
    #[test]
    fn glob_request_roundtrip_full() {
        let req = GlobRequest {
            cwd: Some(PathBuf::from("/workspace")),
            paths: vec![PathBuf::from("src/"), PathBuf::from("tests/")],
            exclude: Some("target/**".to_string()),
            page: 3,
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
        assert_eq!(parsed.page, 3);
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
        assert_eq!(req.page, 1);
        assert!(!req.include_gitignored);
        assert!(!req.include_hidden);
    }

    /// `GlobResponse` roundtrips through JSON.
    #[test]
    fn glob_response_roundtrip() {
        let resp = GlobResponse {
            output: "src/\n  main.rs (42 lines)".to_string(),
            paths: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: GlobResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.output, "src/\n  main.rs (42 lines)");
        assert!(parsed.paths.is_none());
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
