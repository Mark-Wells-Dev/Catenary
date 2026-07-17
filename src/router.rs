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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};

use crate::bridge::EditingGuardrail;
use crate::bridge::HookRouter;
use crate::bridge::filesystem_manager::Root;
use crate::bridge::session::Session;
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

// ── IPC method for CLI search commands ─────────────
//
// No query envelope types remain here: `catenary grep` cut over to the
// streamed hitstream engine in ws43-02 and `catenary glob` in ws43-03
// (`METHOD_HITSTREAM` below); the `tool/grep` and `tool/glob` executor arms
// retired with them. With the `tool/glob` arm went its per-search firehose
// telemetry (the `search`-span shard, `glob/<ts>_<uuid>.jsonl`) — exactly as
// grep's went in ws43-02: annotation-batch traffic is instrumented on the
// hitstream arm instead.

/// IPC method string for the ws43 hit-batch annotation stream.
///
/// The CLI opens this on the existing socket, sends its method line, then streams
/// [`crate::hitstream::HitFrame`] batches; the daemon annotates each under budget
/// and streams [`crate::hitstream::AnnotationFrame`] batches back. An old daemon
/// that predates this method never matches the arm and falls through to the
/// unknown-method tail — the CLI reads no recognizable annotation frame and
/// degrades to the unannotated stream, the same fallback as daemon-absent. The
/// string is owned by the protocol module ([`crate::hitstream::HITSTREAM_METHOD`])
/// and re-exported here.
pub const METHOD_HITSTREAM: &str = crate::hitstream::HITSTREAM_METHOD;

/// Unlinks two bound socket files unless disarmed first.
///
/// The boot-abort cleanup for [`DaemonSockets`] (bug 111). Held as a field so
/// [`DaemonSockets`] itself needs no `Drop` — its plain listener fields can be
/// moved out by [`SessionManager::from_sockets`] with no `Option`/panic dance —
/// and the socket-file lifetime rides this guard instead. A live drop (an
/// aborted boot) unlinks both files; [`disarm`](Self::disarm) suppresses that
/// once a [`SessionManager`] has taken over the lifetime.
#[cfg(unix)]
struct SocketCleanupGuard {
    mcp_path: PathBuf,
    ipc_path: PathBuf,
    armed: bool,
}

#[cfg(unix)]
impl SocketCleanupGuard {
    /// Disarm the guard so [`Drop`] leaves the socket files in place.
    const fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            // A boot aborted after the bind but before a SessionManager took
            // ownership. Unlink both socket files so a subsequent client gets
            // the quiet "no daemon running" arm (os error 2), not the
            // "unreachable" storm (os error 111) — bug 111.
            let _ = std::fs::remove_file(&self.mcp_path);
            let _ = std::fs::remove_file(&self.ipc_path);
        }
    }
}

/// Pre-bound MCP and IPC socket listeners.
///
/// Returned by [`bind_daemon_sockets`] for early socket binding in daemon
/// mode. Pass to [`SessionManager::from_sockets`] once the tool handler
/// is ready.
///
/// # Boot-abort cleanup (bug 111)
///
/// Sockets are bound *first* in daemon startup — before config load and LSP
/// spawning — so bridge proxies can connect while heavy init proceeds. That
/// window means any abort between the bind and [`SessionManager::from_sockets`]
/// (an invalid config today, anything else tomorrow) would unwind through this
/// value while the socket files stay on disk with nobody listening — the
/// `socket exists, connection refused` (os error 111) strand every later client
/// then reports. So this value carries a [`SocketCleanupGuard`] that unlinks both
/// socket files on any drop path. [`SessionManager::from_sockets`] disarms the
/// guard as it takes ownership, because the resulting [`SessionManager`] owns the
/// socket lifetime from that point (its own `Drop` unlinks them).
#[cfg(unix)]
pub struct DaemonSockets {
    /// MCP socket listener.
    mcp_listener: tokio::net::UnixListener,
    /// General-purpose IPC socket listener.
    ipc_listener: tokio::net::UnixListener,
    /// Filesystem path of the MCP socket.
    mcp_path: PathBuf,
    /// Filesystem path of the IPC socket.
    ipc_path: PathBuf,
    /// Boot-abort cleanup: unlinks the two files on drop unless disarmed.
    guard: SocketCleanupGuard,
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
    let sockets = bind_daemon_sockets_at(&mcp_socket_path(), &socket_path())?;

    // A live daemon means the IPC socket is reachable again: clear the
    // "unreachable" onset stamp so a future strand notifies fresh (bug 111).
    // Only the real daemon bind clears it — the path-explicit helper stays
    // side-effect-free so a unit test binding at a tempdir path never touches
    // the real user's runtime-dir stamp.
    crate::notify::UnreachableStamp::new().clear();

    info!(
        source = Source::DaemonLifecycle.as_str(),
        mcp_path = %sockets.mcp_path.display(),
        ipc_path = %sockets.ipc_path.display(),
        "daemon sockets bound",
    );

    Ok(sockets)
}

/// Binds the MCP and IPC sockets at explicit paths, arming the boot-abort
/// cleanup guard (bug 111) but with no notification/stamp side effects.
///
/// The path-explicit core of [`bind_daemon_sockets`]. Kept separate so unit
/// tests can bind into a tempdir without clearing the real user's onset stamp.
///
/// # Errors
///
/// Returns an error if a parent directory cannot be created or either socket
/// cannot be bound.
fn bind_daemon_sockets_at(mcp_path: &Path, ipc_path: &Path) -> Result<DaemonSockets> {
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

    Ok(DaemonSockets {
        mcp_listener,
        ipc_listener,
        guard: SocketCleanupGuard {
            mcp_path: mcp_path.to_path_buf(),
            ipc_path: ipc_path.to_path_buf(),
            armed: true,
        },
        mcp_path: mcp_path.to_path_buf(),
        ipc_path: ipc_path.to_path_buf(),
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

/// Extract the answer desk's read target from a forwarded `PermissionRequest`
/// payload (misc 201).
///
/// Returns `(tool_name, target_path)` when the payload names a read-class tool
/// (`Read`/`Grep`/`Glob`) with a resolvable path field, else `None` (the desk
/// answers nothing). Claude Code's `PermissionRequest` carries `tool_name` and a
/// `tool_input` object whose path field is `file_path` (Read) or `path`
/// (Grep/Glob). A relative path is resolved against the payload's `cwd`.
#[cfg(unix)]
fn permission_read_target(hp: &serde_json::Value) -> Option<(String, PathBuf)> {
    let tool_name = hp.get("tool_name").and_then(|v| v.as_str())?;
    if crate::answer_desk::classify_tool(tool_name) != crate::answer_desk::ToolClass::Read {
        return None;
    }
    let tool_input = hp.get("tool_input")?;
    let raw_path = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("path"))
        .and_then(|v| v.as_str())?;

    let path = Path::new(raw_path);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = hp
            .get("cwd")
            .and_then(|v| v.as_str())
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        cwd.join(path)
    };
    Some((tool_name.to_string(), abs))
}

/// The tool-input path key the answer desk pins the realpath under, for a given
/// read-class tool: `file_path` for `Read`, `path` for the host `Grep`/`Glob`
/// prompts. Defaults to `file_path`.
#[cfg(unix)]
fn permission_input_key(tool_name: &str) -> &'static str {
    match tool_name {
        "Grep" | "Glob" => "path",
        _ => "file_path",
    }
}

/// Build the answer desk's declared [`ReadScope`](crate::answer_desk::ReadScope)
/// from the live tracked roots and the user config (misc 201, decision 031).
///
/// Declared scope = the tracked workspace roots (what the daemon actually serves)
/// ∪ their configured companions ∪ the agents-class worktree base ∪ the
/// `[permissions] always_read` prefixes. Every prefix is canonicalized inside
/// [`ReadScope::new`].
///
/// **Mount state never converts into a desk answer (decision 031):** roots held
/// ONLY by `ephemeral:*` contributors are excluded — an agent-triggered query
/// automount must not quiet-allow the reads that follow it (agent-reachable =
/// self-grantable, the exact loophole the ruling closes). A root a pin shares
/// with a session contribution stays in scope through that contribution. Known
/// residual, flagged for a ruling: `catenary pin` registers under the shared
/// `hook` contributor, indistinguishable from session-cwd contributions here, so
/// a bare pin still confers scope until pins get their own contributor key.
#[cfg(unix)]
fn build_read_scope(
    tracker: Option<&RootTracker>,
    config: &crate::config::Config,
) -> crate::answer_desk::ReadScope {
    let served: Vec<PathBuf> = tracker
        .map(RootTracker::list_roots)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, sources)| !root_is_ephemeral(sources))
        .map(|(path, _)| path)
        .collect();

    // Companions ride the same declared scope as their parent roots. Expand the
    // served roots with their configured companions (a no-op when the feature is
    // off); the served roots themselves are always kept first.
    let mut prefixes = match config.companion_rules() {
        Some(rules) => crate::companions::expand_companions(served, rules),
        None => served,
    };

    // The agents-class worktree base: a dispatched agent's own worktree is
    // always reachable to it, so reading within the base is declared scope.
    prefixes.push(crate::paths::agents_worktrees_dir());

    let permissions = config.permissions();
    let always_read: Vec<PathBuf> = permissions
        .always_read
        .iter()
        .map(|s| PathBuf::from(crate::bridge::expand_tilde(s)))
        .collect();

    crate::answer_desk::ReadScope::new(&prefixes, &always_read)
}

/// Resolve the answer desk's decision for a forwarded `PermissionRequest` payload
/// (misc 201, decision 031).
///
/// Returns `(decision, tool_name)`, or `None` when the desk answers nothing (a
/// write-class tool or an unresolvable read target).
///
/// The target realpath is canonicalized at ingestion ([`canonicalize_lenient`])
/// so a symlinked-prefix alias gets the same verdict as the canonical spelling.
/// The `always_read` first-allow promotion is decided here against the per-session
/// [`PromotedPrefixes`] ledger — only the first allow under a prefix promotes.
#[cfg(unix)]
fn resolve_permission_decision(
    hp: &serde_json::Value,
    tracker: Option<&RootTracker>,
    config: &crate::config::Config,
    promoted: &PromotedPrefixes,
    session_id: &str,
) -> Option<(crate::answer_desk::Decision, String)> {
    let (tool_name, target) = permission_read_target(hp)?;
    let realpath = crate::answer_desk::canonicalize_lenient(&target);

    let scope = build_read_scope(tracker, config);
    let denylist = crate::answer_desk::SensitiveDenylist::load(&config.permissions().deny_paths);

    // Two-phase for the `always_read` promotion: first classify with `false`, and
    // only if it lands `AlwaysReadAllow` do we consult the ledger to set `promote`
    // — so a sensitive-deny or a non-always-read allow never touches the ledger.
    let decision = crate::answer_desk::decide_read(&realpath, &scope, &denylist, false);
    let decision = match decision {
        crate::answer_desk::Decision::AlwaysReadAllow {
            realpath,
            prefix,
            promote: _,
        } => {
            let first = promoted.promote(session_id, &prefix);
            crate::answer_desk::Decision::AlwaysReadAllow {
                realpath,
                prefix,
                promote: first,
            }
        }
        other => other,
    };
    Some((decision, tool_name))
}

/// Emit the loud-allow recording for an out-of-scope read (misc 201, awareness
/// via recording, not denial).
///
/// A [`LoudAllow`](crate::answer_desk::Decision::LoudAllow) is allowed AND
/// recorded: a `warn!` with structured fields lands both in the firehose and as
/// a TUI health finding, so the maintainer's morning report shows the
/// declare-it signal. Every other decision records nothing.
#[cfg(unix)]
fn record_loud_read(decision: &crate::answer_desk::Decision, session_id: &str) {
    if let crate::answer_desk::Decision::LoudAllow { realpath } = decision {
        tracing::warn!(
            source = Source::HookDispatch.as_str(),
            session_id = %session_id,
            path = %realpath.display(),
            "answer desk: allowed a read outside declared scope — declare this directory to silence it",
        );
    }
}

/// Record ONE read-action event for an unprompted host read (misc 201, "record
/// ALL reads — the action, not the content").
///
/// Fires from the `pre-tool/editing-state` dispatch for every read-class host
/// tool (`Read`/`Grep`/`Glob` per [`crate::answer_desk::classify_tool`]) — the
/// gap the answer desk never sees: the Read tool auto-allowed in a working
/// directory, and host `Grep`/`Glob` when they aren't denied, both pass the
/// `PreToolUse` leg untouched and unrecorded today. This closes it: one compact
/// `info!` naming the ACTION — tool, target path, session, agent, cwd — never any
/// file content, so the firehose carries the complete read record and the morning
/// report derives its signals from it.
///
/// **Severity is `info!` by ruling — firehose only.** It must never be `warn!`
/// (a TUI finding) or `error!` (a desktop interrupt): a read is the highest-
/// frequency tool action, so recording is silent archival, not a surfaced
/// condition. The answer desk's out-of-scope `warn!` on PROMPTED reads
/// ([`record_loud_read`]) is a separate, existing surfacing and is unaffected.
///
/// Recording only — this never changes a decision, and it reads the path from the
/// forwarded `host_payload` (`tool_input.file_path` / `tool_input.path`, cwd-
/// resolved) exactly as [`permission_read_target`] does, so a read with no
/// resolvable target (nothing to name) records nothing.
#[cfg(unix)]
fn record_read_action(raw: &serde_json::Value, session_id: &str) {
    let Some(hp) = raw.get("host_payload") else {
        return;
    };
    let Some((tool_name, target)) = permission_read_target(hp) else {
        return;
    };
    let agent_id = raw.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
    let cwd = hook_cwd(raw).unwrap_or("");
    tracing::info!(
        source = Source::HookDispatch.as_str(),
        session_id = %session_id,
        agent_id = %agent_id,
        tool = %tool_name,
        path = %target.display(),
        cwd = %cwd,
        "read action",
    );
}

/// The `SessionStart` project-config setup nudge owed for this hook, or `None`
/// (misc 202).
///
/// Walks the session's own roots (from the `SessionStart` host payload —
/// [`extract_session_roots`], since the `RootTracker` is not yet populated at this
/// seam), and for the first root that both (a) is owed a nudge
/// ([`crate::lsp::project_config::nudge_line`] — a marked project whose server has
/// a config convention whose file is absent) and (b) has not already been nudged
/// this daemon lifetime ([`ProjectConfigNudges`]), records the root and returns
/// its pointer. Every later `SessionStart` on the same root returns `None` — a
/// doorbell, not a nag. Roots are canonicalized so the ledger key matches across
/// symlinked spellings.
#[cfg(unix)]
fn session_start_project_config_nudge(
    ctx: &HookDispatchContext,
    raw: &serde_json::Value,
) -> Option<String> {
    // Gate on the same announce decision the CLI uses (a `resume` restores the
    // prior transcript, so its `SessionStart` surfaces no context). Marking a root
    // the CLI would then drop would silently burn its one shot, so the daemon only
    // computes+marks on an announcing source. `source` rides `host_payload`.
    let source = raw
        .get("host_payload")
        .and_then(|hp| hp.get("source"))
        .and_then(serde_json::Value::as_str);
    if source == Some("resume") {
        return None;
    }
    let config = &ctx.primary.config;
    for root in extract_session_roots(raw) {
        let path = Path::new(&root);
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(line) = crate::lsp::project_config::nudge_line(&canonical, config) {
            // Mark ON FIRE: a root owed no nudge never consumes its "once", so a
            // later run after the project gains a server still surfaces it; a root
            // that fired is silent forever this daemon lifetime.
            if ctx.project_config_nudges.mark(&canonical) {
                return Some(line);
            }
        }
    }
    None
}

/// Detect-and-kick background auto-installs for this `SessionStart` (lsm 05),
/// returning the joined user-visible announcement lines, or `None` when
/// nothing kicked.
///
/// Gated on the user-config-only `[servers] auto_install` opt-in (default
/// `false` — detection runs nothing). Detection reads the session's own roots
/// from the `SessionStart` host payload ([`extract_session_roots`], the misc-202
/// seam: the `RootTracker` is not yet populated here) and asks the
/// [`crate::auto_install::AutoInstaller`] which blessed servers those roots
/// want but cannot spawn. Each missing server is **kicked as a background
/// task** — the dispatch is a spawn, never an await, so session start returns
/// immediately whether or not an install runs. Install completion fires the
/// pin pre-warm machinery (`spawn_all`, the same fire-and-forget leg
/// `tool/roots-add` runs) so coverage arrives promptly for every live mounted
/// root whose markers match.
///
/// Announcements are per actual kick: a server already in flight (a duplicate
/// session start) announces nothing, so the user is told about every
/// auto-install exactly once per attempt.
#[cfg(unix)]
fn session_start_auto_install(
    ctx: &HookDispatchContext,
    raw: &serde_json::Value,
) -> Option<String> {
    let config = &ctx.primary.config;
    if !config.auto_install() {
        return None;
    }
    let roots: Vec<PathBuf> = extract_session_roots(raw)
        .into_iter()
        .map(|root| {
            let path = PathBuf::from(root);
            path.canonicalize().unwrap_or(path)
        })
        .collect();
    if roots.is_empty() {
        return None;
    }
    let missing = ctx.auto_installer.detect(&roots, config);
    let mut lines = Vec::new();
    for server in &missing {
        let primary = ctx.primary.clone();
        let kicked = ctx.auto_installer.kick(server, move || {
            // Install completion is a coverage change: run the same
            // fire-and-forget `spawn_all` pre-warm a `catenary pin` runs
            // (`sync_roots`' prewarm leg), so the new server spawns for every
            // live mounted root whose markers match rather than lazily on the
            // next query.
            tokio::spawn(async move { primary.spawn_all().await });
        });
        if kicked {
            lines.push(crate::auto_install::announce_line(server));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
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

// The `SearchLimiter` (misc 140 phase 2's daemon-wide walk bound) retired with
// the ws43-03 cutover: no daemon-side query walk remains for either verb, so
// there is nothing left to bound — annotation batches are already granular and
// budgeted per batch.

/// Per-root diagnose admission control (misc 197 stage 1, re-keyed to the root in
/// root-ownership stage 3).
///
/// The host harness auto-backgrounds a slow `catenary diagnostics` and retries
/// it; each retry opens a fresh daemon connection, so N concurrent rounds can
/// stack for ONE root. Left unbounded they all fan out to the shared LSP pool
/// at once, and — as the beta sighting showed — the whole daemon can go quiet
/// for an extended stretch. This registry admits one round per ROOT at a time: a
/// second same-root round waits for the in-flight one to finish, then runs its
/// own (with a one-line note), rather than stacking a concurrent execution.
///
/// It is the misc-191 in-flight-marker shape (`Mutex<HashMap<Key, Arc<Notify>>>`
/// with an RAII guard) transplanted from the cold-spawn seat to the diagnose
/// seat, keyed by the ROOT string (the serve resolves the diagnosed files' lock
/// root — no identity below the hook). Same-root rounds serialize; DIFFERENT
/// roots never collide (distinct keys). The one-cook-per-kitchen durable lock
/// means a root has a single editor, so per-root serialization is the natural
/// bound (the earlier per-identity keying is superseded).
///
/// Cloneable: the `Arc` inner map is shared across every hook-connection handler
/// (each connection gets a `HookDispatchContext` clone).
#[cfg(unix)]
#[derive(Clone, Default)]
struct DiagRoundRegistry {
    /// One `Notify` per editing identity with a round in flight. Present ⇒ a
    /// round is executing under that key; absent ⇒ the seat is free. Waiters
    /// clone the `Notify` under the lock and await it; the owner's guard-drop
    /// removes the key and wakes them (they then re-check and claim the seat).
    in_flight: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
}

#[cfg(unix)]
impl DiagRoundRegistry {
    /// Claims the round seat for `key`, or waits for the in-flight round to
    /// finish and then claims it — returning a guard whose `Drop` frees the
    /// seat and wakes the next waiter. The `bool` is `true` when this caller
    /// waited on an in-flight round (it followed another), so the dispatch can
    /// prepend the "another diagnose was in flight" note to its receipt.
    ///
    /// Serialization is the deliberate choice over mid-flight joining: each
    /// caller may name a different file set (a scoped pull vs a bare drain) and
    /// owns its own delivery-flag flip and socket write, so a shared single
    /// result cannot serve both. Waiting-then-running keeps every round's
    /// semantics intact while still admitting exactly one at a time.
    /// Whether any diagnose round is currently executing (any identity).
    ///
    /// The claim guard's activity signal (root-ownership stage 2): a `catenary
    /// claim` refuses while a diagnose round is in flight — a diagnosing agent is
    /// demonstrably present, not gone. The registry keys by editing identity, not
    /// root, and a round may diagnose a batch spanning roots, so the honest
    /// (conservative) signal is "any round in flight" rather than a per-root
    /// query. A quick non-blocking peek under the std lock.
    fn any_in_flight(&self) -> bool {
        !self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    async fn claim(&self, key: &str) -> (DiagRoundGuard<'_>, bool) {
        let mut waited = false;
        loop {
            // Get-or-create the marker atomically under the std lock (the
            // misc-191 spawn-marker claim idiom): `or_insert_with` returns the
            // marker already present, or inserts and returns ours. Comparing
            // `Arc` identity tells claim (we inserted) from wait (someone else's
            // was already there) — and the same lock hold gives the wake-safety
            // discipline: the owner cannot notify until it takes this lock to
            // remove the key, so a waiter that clones under the lock is
            // guaranteed to catch the coming wake.
            let ours = Arc::new(tokio::sync::Notify::new());
            let marker = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key.to_string())
                .or_insert_with(|| ours.clone())
                .clone();
            if Arc::ptr_eq(&marker, &ours) {
                // We claimed the seat.
                return (
                    DiagRoundGuard {
                        in_flight: &self.in_flight,
                        key: key.to_string(),
                        notify: ours,
                    },
                    waited,
                );
            }
            // A round is already in flight under this key. Arm the notified
            // future, then re-check under the lock so a wake between the clone
            // and the wait cannot be missed; if the key is already gone, loop
            // straight back to re-claim.
            let notified = marker.notified();
            tokio::pin!(notified);
            let still_pending = {
                let map = self
                    .in_flight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if map.contains_key(key) {
                    notified.as_mut().enable();
                    true
                } else {
                    false
                }
            };
            if still_pending {
                waited = true;
                notified.await;
            }
        }
    }
}

/// RAII lifetime for an in-flight diagnose round (misc 197 stage 1).
///
/// Mirrors the misc-191 [`SpawnMarkerGuard`](crate::lsp) shape: the round owner
/// holds one across the whole `process_files_batched` pipeline. On `Drop` —
/// completion, an early return, or a cancelled/dropped future — it removes the
/// key from the registry and wakes every waiter. Binding the seat's release to
/// the future's lifetime is the failure semantics: a panicking or abandoned
/// round can never wedge the key, so an unstable pipeline never locks a caller
/// out (the same doctrine as the diagnostics debt drop on daemon death, bug 79).
#[cfg(unix)]
struct DiagRoundGuard<'a> {
    in_flight: &'a std::sync::Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    key: String,
    notify: Arc<tokio::sync::Notify>,
}

#[cfg(unix)]
impl Drop for DiagRoundGuard<'_> {
    fn drop(&mut self) {
        // Tiny sync section: remove the seat, then wake waiters. Never held
        // across an await.
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
        self.notify.notify_waiters();
    }
}

/// The one-line note a diagnose receipt carries when its round followed another
/// same-identity round rather than running concurrently (misc 197 stage 1).
///
/// Prepended as the receipt's first line so the caller can tell "the gate held
/// because another of my own rounds was working" from "the gate is silent". It
/// is a result annotation, not a `warn!`/`error!`: overlapping rounds are the
/// host harness's auto-background retry, expected, not an actionable break.
#[cfg(unix)]
const DIAG_FOLLOWED_NOTE: &str = "another diagnose was in flight; this run followed it";

/// The one-line breadcrumb a diagnose receipt leads with on the first serve
/// after a `catenary claim` takeover (root-ownership stage 3, deliverable 7).
///
/// The serve path reads-and-removes the takeover marker
/// ([`crate::lock::take_claim_marker`]) the claim dropped, so the inheriting
/// editor's first receipt announces it is looking at work it took over — one
/// line, then the per-file receipt (or a bare no-debt answer for a claimed paid
/// root). One-shot: the marker is consumed here, so a later serve on the same
/// root (no new claim) does not repeat it.
#[cfg(unix)]
const CLAIMED_RECEIPT_LEAD: &str = "root claimed from a prior editor";

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
    /// Same-identity diagnose admission control (misc 197 stage 1). Admits one
    /// `catenary diagnostics` round per editing identity at a time so the host
    /// harness's auto-background retries can never stack N concurrent rounds for
    /// one agent. Different identities never collide.
    diag_rounds: DiagRoundRegistry,
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
    /// Answer-desk `always_read` promotion ledger (misc 201). Records the FIRST
    /// allow under each declared `always_read` prefix per session, so only that
    /// prompt emits the session-destination `addDirectories` promotion.
    promoted_prefixes: PromotedPrefixes,
    /// Per-root ledger for the `SessionStart` project-config setup nudge (misc
    /// 202). Fires the missing-config pointer once per served root per daemon
    /// instance; a repeat `SessionStart` on the same root is silent.
    project_config_nudges: ProjectConfigNudges,
    /// Background auto-installer for missing blessed servers (lsm 05). The
    /// `SessionStart` dispatch detects missing servers from root markers and
    /// kicks daemon-side background installs through it — the dispatch is a
    /// spawn, never an await, so session-start latency is flat. Owns the
    /// per-server in-flight dedupe, the concurrency cap, and the
    /// once-per-lifetime failure-warn ledger.
    auto_installer: crate::auto_install::AutoInstaller,
    /// Identity→(path, metadata) registry for Catenary-created worktrees
    /// (misc 150). Registered at `worktree-create/log-payload`, rehydrated from
    /// sidecars at startup; anchors the identity-keyed `SubagentStop` reap and the
    /// `worktree-remove` reverse lookup, and carries misc 151's disposal metadata.
    worktree_registry: WorktreeRegistry,
    /// Per-root blocked-on-permission flag for mounted worktree-class roots (misc
    /// 150). Feeds the `blocked` root-state in `catenary worktree ls` (misc 151);
    /// the flag is cleared by qualifying activity under the root and on the next
    /// identity event. Worktree roots are pinned-class — no idle clock (bug 106);
    /// their release edge is the worktrees-dir vanish-watch.
    worktree_mounts: WorktreeMounts,
    /// Live subagents by parent session (tui-rework 03). Recorded at
    /// `SubagentStart`, pruned at `SubagentStop` / `SessionEnd`; shared with the
    /// session board so `state.json` carries subagent sub-rows.
    subagents: SubagentRegistry,
    /// The user config file `pin`/`unpin` persist `[roots] pinned` to (bug 109).
    ///
    /// Resolved once in [`SessionManager::with_session`]: production uses
    /// [`user_config_path`] (`~/.config/catenary/config.toml`); a test injects a
    /// tempdir path via [`SessionManager::config_path_override`] so an in-process
    /// pin can never touch the operator's real config. Threaded into
    /// [`persist_pin`] / [`persist_unpin`] instead of each re-resolving
    /// [`crate::paths::config_dir`], which no in-process test can redirect
    /// (`std::env::set_var` is forbidden under Rust 2024).
    user_config_path: PathBuf,
}

/// A staged hook→CLI handoff, deposited under a [`HandoffKey`] by the
/// `PreToolUse` hook and consumed by the matching CLI command.
///
/// The payload (see [`HandoffPayload`]) is *data-back*: the surviving `claim`
/// payload carries the rendered answer the identity-less `catenary claim` CLI
/// drains (root-ownership stage 3 demolished the `diagnostics` payload with the
/// two-phase identity handoff).
///
/// Dropping this struct drops the owned semaphore permit, releasing the key's
/// serialization lock for the next same-key stage.
struct HandoffContext {
    /// The staged payload, keyed by direction.
    payload: HandoffPayload,
    /// Owned semaphore permit — dropped when the `HandoffContext`
    /// is dropped (slot consumed or timeout), releasing the per-key lock.
    /// Never read directly; held purely for RAII drop semantics.
    #[allow(dead_code, reason = "RAII guard — held for drop, not read")]
    permit: tokio::sync::OwnedSemaphorePermit,
}

/// The payload of a staged [`HandoffContext`].
///
/// Root-ownership stage 3 demolished the identity-correlation `Diagnostics`
/// payload: diagnostics no longer stages an identity-keyed batch snapshot — the
/// daemon serves against the durable ledger by pure path algebra. The one
/// surviving payload is `Claim`, re-seated on the handoff machinery as the claim
/// answer transport (identity lives at the hook, root-ownership stage 2).
enum HandoffPayload {
    /// `claim` — *data-back*: the `PreToolUse` hook performs the atomic owner-file
    /// rename (identity lives at the hook, root-ownership stage 2) and stages the
    /// rendered answer; the identity-less `catenary claim` CLI drains it. The
    /// answer is fully rendered hook-side so the CLI is a pure printer.
    Claim {
        /// The rendered claim answer (`claimed <root> …`), printed verbatim by
        /// the CLI.
        answer: String,
    },
}

/// Correlation key for the hook→CLI handoff — the catenary subcommand alone
/// (ADR 014).
///
/// Root-ownership stage 3 demolished the `Diagnostics` key with its
/// identity-correlation handoff (diagnostics now serves against the ledger). The
/// sole surviving key is `Claim`: the hook performs the owner-file rename and
/// stages the rendered answer, the identity-less CLI drains it. The registry
/// stays keyed so the claim flow plugs into the mechanism rather than rebuilding
/// a bespoke one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum HandoffKey {
    /// `catenary claim` — data-back: the hook performs the owner-file rename and
    /// stages the rendered answer, the CLI drains it (root-ownership stage 2).
    Claim,
}

impl HandoffKey {
    /// Every handoff key — used to eagerly create the per-key semaphores.
    /// The registry stays keyed so the claim flow plugs into the mechanism
    /// rather than rebuilding it.
    const ALL: [Self; 1] = [Self::Claim];
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
/// - `"seed:env"` — the daemon's boot roots from `CATENARY_ROOTS` (misc 192),
///   registered once at boot so the env seed is a first-class contributor: every
///   re-sync rebuilds the same union (the seed survives any pin/unpin) and the
///   seed has an honest roots-board presence instead of a phantom membership.
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

    /// Returns every contributor key whose root set contains `root`, whatever
    /// its prefix (bug 93).
    ///
    /// [`contributors_with_prefix`](Self::contributors_with_prefix) filters by
    /// key namespace; this filters by the root a contributor declares — the
    /// primitive a full root retirement needs, since a landed worktree may be
    /// held by its `worktree:` mount *and* an `ephemeral:`/`hook`/`mcp:`
    /// contributor at once, and every one must let go for the root to leave the
    /// union (and its per-root servers to shut down). Semantics-free: a pure
    /// query over the provenance index.
    fn contributors_of_root(&self, root: &Path) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contributors
            .iter()
            .filter(|(_, roots)| roots.contains(root))
            .map(|(contributor, _)| contributor.clone())
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

/// The host cwd carried by a hook payload, from either the top-level `cwd`
/// (`PreToolUse`, `Stop`) or the nested `host_payload.cwd` (the forwarded raw
/// event), or `None` when the hook forwards neither (root-ownership 04).
///
/// Every hook carries cwd, and the one hook seam uses it to feed the single
/// activity model for every mount lifetime — the worktree kept countdown and the
/// ephemeral idle clock both reset when a hook resolves into their root. Reading
/// both locations keeps the seam host-agnostic: whichever field the surface
/// populated wins, top-level first.
#[cfg(unix)]
fn hook_cwd(raw: &serde_json::Value) -> Option<&str> {
    raw.get("cwd")
        .and_then(|v| v.as_str())
        .or_else(|| {
            raw.get("host_payload")
                .and_then(|hp| hp.get("cwd"))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
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

/// The user config file (`~/.config/catenary/config.toml`) the persisted pin
/// list lives in (misc 175).
///
/// Resolves through the same [`crate::config::ConfigLayer::User`] path the
/// guided-mutation writer uses, so a `pin`/`unpin` config write and the boot
/// restore agree on which file carries `[roots] pinned`.
#[cfg(unix)]
fn user_config_path() -> PathBuf {
    crate::paths::config_dir()
        .join("catenary")
        .join("config.toml")
}

/// Persist a `catenary pin` to the user config's `[roots] pinned` list (misc
/// 175), the durability leg of the runtime pin.
///
/// The runtime pin (the `hook` contributor) has already been applied by the
/// time this runs; the config write is what survives a daemon restart. Failure
/// is non-fatal to the pin — the root is pinned live regardless — so a write
/// error is logged at `warn!()` (a TUI health finding, no interrupt) rather than
/// failing the command: the operator's live pin still holds, only its
/// persistence is impaired.
///
/// `config_path` is the file to write, injected from
/// [`HookDispatchContext`]'s resolved config path (bug 109). Production resolves
/// it once via [`user_config_path`]; in-process tests inject a tempdir path so a
/// test pin can never reach the operator's real
/// `~/.config/catenary/config.toml`. Rust 2024 forbids `std::env::set_var` in
/// this crate, so an in-process test cannot redirect `config_dir()` — the write
/// target must be injected, not env-resolved, or the test writes the user's file.
#[cfg(unix)]
fn persist_pin(config_path: &Path, canonical: &Path) {
    match crate::config::pin_config(config_path, canonical) {
        Ok(true) => debug!(
            source = Source::DaemonDispatch.as_str(),
            path = %canonical.display(),
            "persisted pin to user config",
        ),
        Ok(false) => debug!(
            source = Source::DaemonDispatch.as_str(),
            path = %canonical.display(),
            "pin already present in user config — no config change",
        ),
        Err(e) => warn!(
            source = Source::DaemonDispatch.as_str(),
            path = %canonical.display(),
            "failed to persist pin to user config (the root is pinned live regardless): {e:#}",
        ),
    }
}

/// Drop a `catenary unpin` from the user config's `[roots] pinned` list (misc
/// 175), mirroring [`persist_pin`].
///
/// A missing config file or an absent entry is a benign no-op. A write failure
/// is non-fatal and logged at `warn!()`. Returns `true` when an entry was
/// removed — the caller folds this into the `unpin` outcome so a config-only
/// entry (a pin whose path was missing at boot) still reports success.
///
/// `config_path` is injected (bug 109) exactly as in [`persist_pin`].
#[cfg(unix)]
fn persist_unpin(config_path: &Path, canonical: &Path) -> bool {
    match crate::config::unpin_config(config_path, canonical) {
        Ok(true) => {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                path = %canonical.display(),
                "removed pin from user config",
            );
            true
        }
        Ok(false) => {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                path = %canonical.display(),
                "pin not present in user config — no config change",
            );
            false
        }
        Err(e) => {
            warn!(
                source = Source::DaemonDispatch.as_str(),
                path = %canonical.display(),
                "failed to remove pin from user config: {e:#}",
            );
            false
        }
    }
}

/// Contributor key for the daemon's `CATENARY_ROOTS` boot seed (misc 192).
///
/// A single fixed key (not a `{...}`-parameterized namespace like `mcp:{fd}` or
/// `worktree:*`) — there is exactly one env seed per daemon. Registered once at
/// boot in [`register_env_seed`], so every re-sync (a pin via `tool/roots-add`,
/// an MCP disconnect, a worktree reap) rebuilds a union that still carries the
/// seed instead of silently evicting it.
#[cfg(unix)]
const SEED_ENV_CONTRIBUTOR: &str = "seed:env";

/// Register the daemon's `CATENARY_ROOTS` boot roots as the `seed:env` tracker
/// contributor (misc 192).
///
/// The session is born (in `main::run_daemon_main`) with the canonicalized
/// `CATENARY_ROOTS` set already installed in its `FilesystemManager`/
/// `LspClientManager`, but those roots are *not* tracker contributors: a later
/// re-sync that rebuilds the served union from `tracker.global_roots_rich()`
/// (e.g. `tool/roots-add`) would replace the session's roots with the
/// contributor union and silently drop the seed. Registering the seed here
/// closes that gap — the seed survives any pin/unpin and gains an honest
/// roots-board line instead of a phantom membership.
///
/// Zero-cost, mirroring [`restore_pinned_roots`]: `set_roots` births a
/// config-loaded [`Root`] per path but spawns nothing (the session's own
/// `spawn_all` at boot already covers these roots), and no `sync_roots` push is
/// needed — the session already serves the seed. An empty seed (never the case:
/// `CATENARY_ROOTS` defaults to `["."]`, canonicalized) is a benign no-op.
#[cfg(unix)]
fn register_env_seed(tracker: &RootTracker, session: &Arc<Session>) {
    let seed = session.roots();
    if seed.is_empty() {
        return;
    }
    let count = seed.len();
    tracker.set_roots(SEED_ENV_CONTRIBUTOR, seed);
    info!(
        source = Source::DaemonLifecycle.as_str(),
        count, "registered CATENARY_ROOTS boot seed as tracker contributor",
    );
}

/// Restore persisted pins at daemon boot (misc 175): re-add each `[roots]
/// pinned` config entry as a `hook` contributor so a pin survives a restart.
///
/// Each entry is tilde-expanded then canonicalized (the tracker canonicalizes
/// every root, so this yields one spelling per root — matching a hand-authored
/// `~`-prefixed spelling and the daemon's own pin-time canonical form). An entry
/// whose path is **missing on disk** (deleted repo, unmounted volume) is left in
/// the config and NOT tracked: the doctor missing-pin finding surfaces it, and
/// Catenary never rewrites the user's config outside an explicit pin/unpin, so a
/// transiently absent mount stays pinned.
///
/// Restore is zero-cost. `add_roots` births a config-loaded [`Root`] (a tracker
/// entry + a roots-board line) but spawns nothing: on a fresh daemon no language
/// is active, so `spawn_for_added_roots` (the warm-language spawn leg) fires for
/// none of them, and the first-touch lazy spawn pays only when a root is used.
/// The single `sync_roots` push (fire-and-forget on the session runtime, like the
/// startup `spawn_all`) makes the roots resolvable by tool calls without eager
/// spawning.
#[cfg(unix)]
fn restore_pinned_roots(tracker: &RootTracker, session: &Arc<Session>) {
    let mut restored: Vec<PathBuf> = Vec::new();
    for entry in session.config.pinned_roots() {
        let expanded = crate::bridge::expand_tilde(entry);
        let expanded = PathBuf::from(expanded);
        match expanded.canonicalize() {
            Ok(canonical) => restored.push(canonical),
            Err(_) => {
                // Missing at boot — keep the config entry (never pruned), let the
                // doctor missing-pin finding surface it. A transiently absent
                // mount stays pinned.
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    entry = %entry,
                    "pinned root missing at boot — kept in config, not tracked \
                     (see `catenary doctor`)",
                );
            }
        }
    }

    if restored.is_empty() {
        return;
    }

    tracker.add_roots("hook", &restored);
    info!(
        source = Source::DaemonLifecycle.as_str(),
        count = restored.len(),
        "restored persisted pins from user config",
    );

    // Push the restored union into the FilesystemManager/LspClientManager so a
    // first-touch tool call resolves the root, WITHOUT the eager pre-warm:
    // `sync_roots_no_prewarm` skips the fire-and-forget `spawn_all`, so a restored
    // pin spawns no server (its `spawn_for_added_roots` leg is a no-op on a fresh
    // daemon). The lazy first-touch path is preserved. Fire-and-forget on the
    // session runtime, mirroring the startup spawn.
    let session = Arc::clone(session);
    let global = tracker.global_roots_rich();
    session.runtime.clone().spawn(async move {
        if let Err(e) = session.sync_roots_no_prewarm(global).await {
            warn!(
                source = Source::DaemonLifecycle.as_str(),
                "root sync after pin restore failed: {e:#}",
            );
        }
    });
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

/// Answer-desk `always_read` promotion ledger (misc 201).
///
/// Keyed on `(session_id, prefix)`: [`promote`](Self::promote) records the pair
/// and returns `true` iff it was not already present — the FIRST allow under a
/// declared `always_read` prefix, which emits the session-destination
/// `addDirectories` promotion so subsequent reads of that tree are prompt-free
/// natively. Every later allow under the same prefix returns `false` (no
/// re-promotion). In-memory only — a daemon restart re-promotes on the next read,
/// which is harmless (session destination re-adds the same working directory).
#[cfg(unix)]
#[derive(Clone, Default)]
struct PromotedPrefixes {
    inner: Arc<std::sync::Mutex<HashSet<String>>>,
}

#[cfg(unix)]
impl PromotedPrefixes {
    fn new() -> Self {
        Self::default()
    }

    /// Records `(session_id, prefix)` and returns `true` iff it was not already
    /// present — the first allow under this prefix in this session.
    fn promote(&self, session_id: &str, prefix: &Path) -> bool {
        let key = format!("{session_id}::{}", prefix.display());
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key)
    }
}

/// Per-root ledger for the `SessionStart` project-config setup nudge (misc 202).
///
/// The nudge points an agent at a served root's missing language-server config
/// file (rust-analyzer → `rust-analyzer.toml`), so its editor/receipt lint+feature
/// surface can match its build. It fires **once per root per daemon instance** — a
/// doorbell, not an alarm: a second `SessionStart` resolving to the same root is
/// silent. The ledger lives for the daemon's lifetime; a restart forgets it (a
/// bounded, acceptable one-time re-fire on the next `SessionStart`, analogous to
/// [`FirstSightings`]). Memory is one path per nudged root.
#[cfg(unix)]
#[derive(Clone, Default)]
struct ProjectConfigNudges {
    inner: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
}

#[cfg(unix)]
impl ProjectConfigNudges {
    fn new() -> Self {
        Self::default()
    }

    /// Records `root` and returns `true` iff it was **not** already present — i.e.
    /// this root has not been nudged this daemon lifetime, so the pointer should be
    /// surfaced now. Every later call for the same root returns `false`.
    fn mark(&self, root: &Path) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.to_path_buf())
    }
}

/// The auto-mount verdict for one touched path (ephemeral-roots ticket 02,
/// sensitive-path gate ws43-05).
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum EphemeralMountVerdict {
    /// Mount this canonical enclosing project root.
    Mount(PathBuf),
    /// An enclosing root would have mounted, but the touched path matches the
    /// answer desk's sensitive-path denylist — the conversion is refused
    /// (ws43-05). Carries the root that would have mounted, for the refusal
    /// recording. The search result itself is untouched (decision 025): the hit
    /// still streams, it just stays unenriched because nothing mounted.
    RefusedSensitive(PathBuf),
    /// Nothing to convert: the path is covered by a tracked root, or no
    /// enclosing repository root is detectable.
    NoMount,
}

/// Decides whether a touched path warrants mounting an enclosing ephemeral root.
///
/// Returns [`EphemeralMountVerdict::Mount`] with the canonical enclosing project
/// root iff:
///
/// - the touched path is **not** already inside any tracked root (equal to or
///   under one — the "outside every mounted root" test, which also rejects a
///   path under a mounted sub-root), and
/// - an enclosing project root is detectable by walking repository markers
///   (`.git`/`.svn`/`.hg`/`.jj`) up from the path
///   ([`crate::companions::enclosing_worktree_root`]), and
/// - that root is not itself already tracked, and
/// - the touched path does **not** match the sensitive-path denylist (ws43-05):
///   a sensitive path NEVER converts into a mount — no root registration, no
///   server spawn, no tracker entry — and yields
///   [`EphemeralMountVerdict::RefusedSensitive`] instead. The gate lives in the
///   decision itself, not in a caller, so any future caller inherits it without
///   re-wiring. It governs mount CONVERSION only: an in-root sensitive path is
///   already `NoMount` via the covered test above, and results are never
///   dropped (decision 025 — the walk runs with the user's own permissions).
///
/// `denylist` is the answer desk's own compiled
/// [`crate::answer_desk::SensitiveDenylist`] — one source of truth for the
/// pattern logic, never a fork. `canonical_touched` should be canonicalized by
/// the caller when the path exists so the comparison lines up with the
/// tracker's canonical roots (and the denylist's path-spelling rule); a glob
/// pattern or not-yet-existing path (which cannot canonicalize) still resolves
/// its enclosing repository root by lexical ancestor walk. Scope guard: only the single
/// enclosing root is returned — never a sibling — and companion templating is
/// never applied to it.
#[cfg(unix)]
fn ephemeral_root_to_mount(
    canonical_touched: &Path,
    tracked: &HashSet<PathBuf>,
    denylist: &crate::answer_desk::SensitiveDenylist,
) -> EphemeralMountVerdict {
    // Already inside a tracked root → covered, no ephemeral mount. The
    // sensitive gate never fires here — it governs mount conversion only.
    if tracked.iter().any(|r| canonical_touched.starts_with(r)) {
        return EphemeralMountVerdict::NoMount;
    }
    let Some(root) = crate::companions::enclosing_worktree_root(canonical_touched) else {
        return EphemeralMountVerdict::NoMount;
    };
    let root = root.canonicalize().unwrap_or(root);
    // Belt-and-suspenders vs the check above (a canonicalization mismatch): the
    // enclosing root is already a tracked root.
    if tracked.contains(&root) {
        return EphemeralMountVerdict::NoMount;
    }
    // The sensitive-path gate (ws43-05): checked exactly where a conversion
    // would otherwise happen, so a covered path or a no-root path never
    // records a spurious refusal.
    if denylist.is_sensitive(canonical_touched) {
        return EphemeralMountVerdict::RefusedSensitive(root);
    }
    EphemeralMountVerdict::Mount(root)
}

/// Delivery-side unlink of durable-lock ledger entries for a served file set
/// (root-ownership stage 2).
///
/// Groups `files` by their resolved lock root ([`crate::lock::resolve_lock_root`])
/// and unlinks each file's `dir/<relpath>.lock` touch entry from that root's
/// on-disk lock ledger. Emptying `dir/` marks the lock **paid**, arming the
/// daemon's paid-idle countdown. Payment is parole, not release — the lock dir
/// survives; only the paid-idle reaper and root retirement remove it. Files
/// outside any repository (no lock root) unlink nothing. Best-effort throughout.
///
/// Every entry's [`crate::lock::UnlinkOutcome`] is traced (bug 120): the
/// delivery unlink used to emit nothing, so a served file whose ledger entry
/// survived delivery — phantom debt the next Stop blocks on — left no evidence
/// of WHERE the payment went missing. Routine outcomes (paid, or served without
/// standing debt) land at `debug!`; an actual unlink failure lands at `info!`
/// (firehose-only — internal diagnostics never interrupt).
#[cfg(unix)]
fn unlink_delivered_locks(files: &[PathBuf]) {
    for (file, outcome) in crate::lock::unlink_delivered_by_root(files) {
        match outcome {
            crate::lock::UnlinkOutcome::Unlinked => debug!(
                source = Source::DaemonDispatch.as_str(),
                path = %file.display(),
                "diagnostics delivery: ledger entry unlinked (paid)",
            ),
            crate::lock::UnlinkOutcome::NoRoot => debug!(
                source = Source::DaemonDispatch.as_str(),
                path = %file.display(),
                "diagnostics delivery: no lock root resolves — unlink skipped",
            ),
            crate::lock::UnlinkOutcome::NoLedger => debug!(
                source = Source::DaemonDispatch.as_str(),
                path = %file.display(),
                "diagnostics delivery: root has no lock dir — unlink skipped",
            ),
            crate::lock::UnlinkOutcome::NoEntry => debug!(
                source = Source::DaemonDispatch.as_str(),
                path = %file.display(),
                "diagnostics delivery: no ledger entry for served file — unlink skipped",
            ),
            crate::lock::UnlinkOutcome::Failed(kind) => info!(
                source = Source::DaemonDispatch.as_str(),
                path = %file.display(),
                cause = ?kind,
                "diagnostics delivery: ledger unlink failed — entry survives as phantom debt (bug 120)",
            ),
        }
    }
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

// ── Worktree-class roots: the kept countdown + blocked display (root-ownership 04) ──

/// How long a KEPT worktree mount survives without a refreshing hook activity
/// before the countdown reaper retires its MOUNT (root-ownership 04).
///
/// On the "kept" signal (subagent stopped, worktree dirty) the mount enters a
/// countdown; any hook resolving into the worktree resets it — pure
/// activity-reset, no pause machinery (the third-round ruling). Matched to
/// [`EPHEMERAL_ROOT_IDLE_TIMEOUT`] and [`crate::lock::PAID_IDLE_TIMEOUT`] so the
/// three idle clocks in the daemon agree — one activity model for every mount
/// lifetime. Comfortably inside the ticket's 5–10 minute band; the reaper sweep
/// ([`EPHEMERAL_ROOT_SWEEP_INTERVAL`]) adds at most one minute of slack, keeping
/// the worst case under 10 minutes.
#[cfg(unix)]
const WORKTREE_KEPT_COUNTDOWN: Duration = Duration::from_mins(7);

/// One worktree root's mount state: the kept countdown and the blocked-display
/// flag (root-ownership 04).
///
/// A LIVE worktree (its subagent still running) is pinned-class — `kept_since` is
/// `None`, so no countdown runs; the vanish-watch is its release edge. On the
/// "kept" signal (subagent stopped, worktree dirty) `kept_since` is armed to the
/// stop instant, and any hook resolving into the worktree refreshes it — the same
/// idle-with-activity-reset [`EphemeralMounts`] uses. The countdown reaper retires
/// the MOUNT (servers, root, lock) once it lapses, never the dirty directory.
#[cfg(unix)]
struct WorktreeClock {
    /// The tracked root path (for `touch_covering`'s prefix test, `mounted_roots`,
    /// the countdown reaper, and logging).
    root: PathBuf,
    /// Blocked-on-permission: a subagent parked at a permission prompt. Feeds the
    /// `blocked` root-state in `catenary worktree ls` (misc 151); marked/cleared
    /// by path (the enclosing worktree of the `PermissionRequest` / activity
    /// cwd) — no identity keying (root-ownership 04, AUDIT #11).
    blocked: bool,
    /// The kept countdown's last-activity instant, or `None` while the worktree is
    /// LIVE (pinned-class, no expiry). Armed on the "kept" signal
    /// ([`arm_countdown`](WorktreeMounts::arm_countdown)); refreshed by any hook
    /// resolving into the worktree; the reaper retires the mount once it lapses
    /// past [`WORKTREE_KEPT_COUNTDOWN`].
    kept_since: Option<Instant>,
}

/// Per-root mount state (kept countdown + blocked display) for mounted
/// worktree-class roots, keyed by contributor (root-ownership 04).
///
/// The worktree analogue of [`EphemeralMounts`], keyed by the **contributor** so
/// a teardown carries the key it will `remove`. Contributors are now uniformly
/// path-shaped (`worktree:{session}:{canonical-path}`) — the identity-shaped
/// `worktree:{session}:{agent_id}` form retired with the registry identity
/// lookups (root-ownership 04, AUDIT #10/#11). Each entry carries the kept
/// countdown (the mount's lifetime once its subagent stops dirty) and the
/// blocked-on-permission display flag.
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

    /// Track a mounted worktree root. Mounting always resets a fresh, LIVE state
    /// (no countdown, unblocked — a re-mount at the same key is a fresh subagent).
    fn track(&self, contributor: &str, root: &Path) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                contributor.to_string(),
                WorktreeClock {
                    root: root.to_path_buf(),
                    blocked: false,
                    kept_since: None,
                },
            );
    }

    /// Refresh every worktree root that encloses `path` on qualifying activity.
    ///
    /// Any hook resolving into a worktree is activity: it clears the
    /// blocked-on-permission flag (the agent resumed) AND — the root-ownership-04
    /// countdown reset — refreshes the kept countdown to `now` when one is armed,
    /// so an attended, actively-touched worktree never idle-expires its mount.
    /// `path` should already be canonicalized so it lines up with the canonical
    /// root keys.
    fn touch_covering(&self, path: &Path, now: Instant) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for clock in inner.values_mut() {
            if path.starts_with(&clock.root) {
                clock.blocked = false;
                if clock.kept_since.is_some() {
                    clock.kept_since = Some(now);
                }
            }
        }
    }

    /// Arm the kept countdown on the worktree mount enclosing `path` (the "kept"
    /// signal — subagent stopped, worktree dirty). Idempotent re-arm to `now`.
    ///
    /// Resolves by path so it needs no identity: the stopping subagent's cwd
    /// resolves to its worktree root, and the enclosing mount enters the
    /// countdown. Returns whether a mount was armed.
    fn arm_countdown(&self, path: &Path, now: Instant) -> bool {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut armed = false;
        for clock in inner.values_mut() {
            if path.starts_with(&clock.root) {
                clock.kept_since = Some(now);
                armed = true;
            }
        }
        drop(inner);
        armed
    }

    /// Mark every worktree root enclosing `path` blocked-on-permission (a
    /// `PermissionRequest` resolving into the worktree). Path-keyed — no identity
    /// (root-ownership 04). Returns the count marked.
    fn mark_blocked_covering(&self, path: &Path) -> usize {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut marked = 0;
        for clock in inner.values_mut() {
            if path.starts_with(&clock.root) {
                clock.blocked = true;
                marked += 1;
            }
        }
        drop(inner);
        marked
    }

    /// The contributor keys whose kept countdown has lapsed past `idle` as of
    /// `now` — the mounts the reaper retires (root-ownership 04).
    ///
    /// Only KEPT mounts (`kept_since = Some`) are candidates; a LIVE worktree
    /// (`None`) is never expired. `saturating_duration_since` is panic-free
    /// against a future `Instant`. Returns `(contributor, root)` pairs so the
    /// reaper can retire the mount and log the root.
    fn expired_countdowns(&self, now: Instant, idle: Duration) -> Vec<(String, PathBuf)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|(contributor, clock)| {
                clock.kept_since.and_then(|since| {
                    (now.saturating_duration_since(since) >= idle)
                        .then(|| (contributor.clone(), clock.root.clone()))
                })
            })
            .collect()
    }

    /// Drop a root's entry (on teardown).
    fn remove(&self, contributor: &str) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(contributor);
    }

    /// Drop every entry whose contributor starts with `prefix` (the `SessionEnd`
    /// sweep).
    fn remove_prefix(&self, prefix: &str) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|contributor, _| !contributor.starts_with(prefix));
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

    /// The kept-countdown last-activity for a contributor, or `None` when the
    /// mount is absent or LIVE (test-only).
    #[cfg(test)]
    fn kept_since(&self, contributor: &str) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(contributor)
            .and_then(|clock| clock.kept_since)
    }
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
    /// Worktrees already surfaced dirty-kept to the parent this daemon lifetime
    /// (bug 91): the `SubagentStop` dirty-kept notice ([`surface_dirty_kept`])
    /// fires **once per worktree path**, so several subagents whose cwd resolves
    /// to the one surviving dirty worktree yield exactly one reminder — not one
    /// per stopping agent. Cleared for a path by [`WorktreeRegistry::prune_missing`]
    /// once its dir is gone, so a worktree recreated at the same path re-surfaces.
    surfaced: Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
}

#[cfg(unix)]
impl WorktreeRegistry {
    fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
            nagged: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            surfaced: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
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

    /// Record that a worktree has been surfaced dirty-kept to the parent; returns
    /// `true` only the first time (bug 91 — one reminder per worktree path, not
    /// per stopping subagent).
    fn mark_surfaced(&self, worktree: &Path) -> bool {
        self.surfaced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(worktree.to_path_buf())
    }

    /// Drop every registration whose worktree directory is gone on disk, clearing
    /// its `nagged`/`surfaced` marks too (bug 91). Returns the pruned paths.
    ///
    /// Ages out the stale tracked entries a landed/removed worktree leaves behind:
    /// a registration (and its once-per-worktree surfacing marks) must not outlive
    /// its directory, or a phantom entry lingers and a path recreated there is
    /// denied a fresh reminder. Clearing the marks with the entry keeps the safety
    /// invariant intact — a worktree that exists and is dirty is always reportable;
    /// only *gone* paths lose their marks.
    fn prune_missing(&self) -> Vec<PathBuf> {
        let gone: Vec<PathBuf> = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let gone: Vec<PathBuf> = inner
                .keys()
                .filter(|path| !path.exists())
                .cloned()
                .collect();
            for path in &gone {
                inner.remove(path);
            }
            gone
        };
        if !gone.is_empty() {
            for path in &gone {
                self.nagged
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(path);
                self.surfaced
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(path);
            }
        }
        gone
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
///
/// The sensitive-path gate (ws43-05): a touched path matching the answer
/// desk's sensitive-path denylist NEVER converts into a mount — no root
/// registration, no server spawn, no tracker entry. The gate is on MOUNT
/// STATE, not on search results (decision 025 — the hit still streams, it
/// simply stays unenriched). The denylist is compiled per call from the same
/// source the answer desk uses (the embedded defaults plus the daemon
/// config's `[permissions] deny_paths`) — mounting is rare, so the per-call
/// load is cheap and stays consistent with the desk's per-dispatch load. A
/// refusal is recorded as an `info!` action event — firehose-only, no TUI
/// finding, no interrupt (an unenriched hit is not urgent).
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
    let denylist =
        crate::answer_desk::SensitiveDenylist::load(&ctx.primary.config.permissions().deny_paths);
    let mut mounted = false;
    for path in touched {
        // Canonicalize when the path exists so the comparison lines up with the
        // tracker's canonical roots; a glob pattern / not-yet-existing path keeps
        // its resolved spelling (its enclosing `.git` still resolves lexically).
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        // Every qualifying activity refreshes the covering ephemeral root's idle
        // clock and the covering worktree's kept countdown, clearing its
        // blocked-on-permission flag too (root-ownership 04 — one activity model).
        mounts.touch_covering(&canonical, now);
        ctx.worktree_mounts.touch_covering(&canonical, now);
        let existing: HashSet<PathBuf> = tracker.global_roots().into_iter().collect();
        match ephemeral_root_to_mount(&canonical, &existing, &denylist) {
            EphemeralMountVerdict::Mount(root) => {
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
            EphemeralMountVerdict::RefusedSensitive(root) => {
                // Recording-only (ws43-05): `info!` is firehose-only per the
                // tracing conventions — never a TUI finding or an interrupt.
                info!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id = %session_id,
                    path = %canonical.display(),
                    root = %root.display(),
                    "sensitive-path gate: refused ephemeral mount for a sensitive \
                     touched path — the hit streams unenriched",
                );
            }
            EphemeralMountVerdict::NoMount => {}
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

/// The daemon-side annotator behind the `tool/hitstream` dispatch arm
/// (ws43-02 grep, ws43-03 glob).
///
/// Wraps the bridge's [`crate::bridge::HitstreamEnricher`] (the retired query
/// executors' LSP enrichment, migrated) with the router-level query
/// auto-mount: before a batch is enriched, [`ensure_ephemeral_mounts`] runs
/// over the batch's distinct canonical hit paths, so an out-of-root batch is
/// served by the freshly-mounted root's server — the same pre-execute mount
/// the retired `tool/grep`/`tool/glob` arms performed, keyed on the canonical
/// paths the batches carry (the CLI canonicalizes at the walk seam).
/// Mount-on-query thus rides the annotation batches for BOTH verbs. The
/// sensitive-path gate (ws43-05) lives inside `ensure_ephemeral_mounts` and
/// rides along unweakened: a refused mount leaves the hit streaming,
/// unenriched.
///
/// The whole call — mount, nudge, enrichment — runs under the annotator's
/// per-batch budget ([`crate::hitstream::ANNOTATION_BATCH_BUDGET`]): a cold
/// mount or a slow settle blows the budget into a pass-through verdict on a
/// complete batch (degrade-only), and later batches find the mount warm.
#[cfg(unix)]
struct HitstreamAnnotator<'a> {
    /// Dispatch context for the auto-mount (root tracker, ephemeral mounts).
    ctx: &'a HookDispatchContext,
    /// The migrated enrichment (pool readiness, WS31 nudge, `#scope` anchors,
    /// weighted outlines).
    inner: crate::bridge::HitstreamEnricher,
}

#[cfg(unix)]
impl crate::hitstream::BatchEnricher for HitstreamAnnotator<'_> {
    /// The walk-level observation nudge (ws43-02 reap parity) delegates
    /// straight to the migrated enricher — no auto-mount here: observations
    /// are coherence bookkeeping for roots already served, not a query that
    /// earns a mount, exactly as the executor's nudge never mounted.
    async fn observe_walk(&self, observed: Vec<(PathBuf, i64)>, reap_scopes: Option<Vec<PathBuf>>) {
        self.inner.observe_walk(observed, reap_scopes).await;
    }

    async fn enrich(
        &self,
        hits: Vec<crate::hitstream::WireHit>,
        observed: Vec<(PathBuf, i64)>,
        weight: Option<crate::hitstream::EnrichmentWeight>,
    ) -> Result<Vec<crate::hitstream::frame::AnnotatedHit>> {
        // The batch's distinct files are the touched paths — dedup keeps the
        // mount pass linear in files, not hits.
        let touched: Vec<PathBuf> = hits
            .iter()
            .map(|h| h.path.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        ensure_ephemeral_mounts(self.ctx, &touched, Instant::now(), "").await;
        self.inner.enrich(hits, observed, weight).await
    }
}

/// Grace window between the MCP connection census reaching zero and the
/// daemon's "last client disconnected" exit (pulse-03).
///
/// Host-driven bridge churn — SIGINT at session transitions, resumes, model
/// switches — drops the census to zero for moments at a time; exiting on the
/// instant zero tore down the warm LSP fleet four times in 25 minutes during
/// the 2026-07-17 incident. The accept loop instead arms this window and
/// exits only if the census is still zero when it expires; a connection
/// arriving during the window disarms it. Ticket 90's abandoned-daemon exit
/// is debounced, not replaced — a genuinely abandoned daemon still exits,
/// one grace window late. Tests inject a smaller window via
/// [`SessionManager::disconnect_grace_override`].
#[cfg(unix)]
const DISCONNECT_GRACE: Duration = Duration::from_mins(1);

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
    /// Grace window for the debounced last-client exit (pulse-03). Defaults
    /// to [`DISCONNECT_GRACE`]; tests shrink it via
    /// [`Self::disconnect_grace_override`] so expiry paths run in
    /// milliseconds.
    disconnect_grace: Duration,
    /// Observability seam for the exit grace window: `true` while the window
    /// is armed (census at zero, exit pending). Written only by
    /// [`Self::accept_loop`]; read by tests to sequence deterministically
    /// against arm/disarm instead of racing the clock.
    grace_armed: AtomicBool,
    /// Receiver for worktree-deletion events from the [`crate::worktree_watch`]
    /// watcher, stashed by [`Self::with_session`] and taken once by
    /// [`Self::spawn_worktree_watch_reaper`]. `None` until `with_session` wires
    /// the watcher (or if the OS watcher couldn't be created).
    worktree_watch_rx: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::worktree_watch::WorktreeDeleted>>,
    >,
    /// Override for the `[roots] pinned` persistence file (bug 109). `None` in
    /// production — [`Self::with_session`] then resolves [`user_config_path`]
    /// (`~/.config/catenary/config.toml`). An in-process test sets it via
    /// [`Self::config_path_override`] to a tempdir path so a test pin/unpin can
    /// never write the operator's real config (the pin write path is otherwise
    /// unredirectable in-process: `std::env::set_var` is forbidden under Rust
    /// 2024, so `config_dir()` always resolves the real `~/.config`).
    config_path_override: Option<PathBuf>,
    /// Override for the background auto-installer (lsm 05). `None` in
    /// production — [`Self::with_session`] then builds the real installer
    /// (seed recipes, live manifest, the real managed home and
    /// process/network seams). An in-process test injects one with stubbed
    /// seams via [`Self::auto_installer_override`] so a dispatch test can
    /// exercise the real background-task path without touching the network,
    /// the toolchain, or the operator's managed home.
    auto_installer_override: Option<crate::auto_install::AutoInstaller>,
    /// Once-per-pairing dedup for the bridge↔daemon version-mismatch interrupt
    /// (ws41-02). Holds the [`catenary_mcp::VersionMismatch::pairing_key`] of
    /// every `(bridge, daemon)` pairing that has already fired its one
    /// `error!()` desktop interrupt this daemon lifetime. Shared into each MCP
    /// connection's bridge-hello callback so repeated session-starts reporting
    /// the same pairing never re-fire; a daemon restart re-verifies (an
    /// acceptable one-time re-fire, not a per-session-start refire). The
    /// persistent surfaces (doctor, board, `SessionStart`) carry the reminder
    /// beneath the single interrupt.
    mismatch_interrupts: Arc<std::sync::Mutex<HashSet<String>>>,
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
    /// immediately. Disarms the [`DaemonSockets`] boot-abort guard as it takes
    /// the listeners: from here on [`SessionManager::drop`] owns unlinking the
    /// socket files (bug 111).
    #[must_use]
    pub fn from_sockets(sockets: DaemonSockets, logging: LoggingServer) -> Self {
        // Take over the socket-file lifetime: disarm the boot-abort guard so its
        // drop leaves the files in place — this SessionManager's own `Drop`
        // unlinks them from here on (bug 111).
        let DaemonSockets {
            mcp_listener,
            ipc_listener,
            mcp_path,
            ipc_path,
            mut guard,
        } = sockets;
        guard.disarm();
        Self {
            mcp_listener,
            ipc_listener,
            mcp_socket_path: mcp_path,
            ipc_socket_path: ipc_path,
            logging,
            connection_count: Arc::new(AtomicUsize::new(0)),
            next_connection_id: Arc::new(AtomicUsize::new(0)),
            hook_ctx: None,
            lsp: None,
            root_tracker: None,
            shutdown: CancellationToken::new(),
            disconnect: Arc::new(tokio::sync::Notify::new()),
            disconnect_grace: DISCONNECT_GRACE,
            grace_armed: AtomicBool::new(false),
            worktree_watch_rx: std::sync::Mutex::new(None),
            config_path_override: None,
            auto_installer_override: None,
            mismatch_interrupts: Arc::new(std::sync::Mutex::new(HashSet::new())),
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
            disconnect_grace: DISCONNECT_GRACE,
            grace_armed: AtomicBool::new(false),
            worktree_watch_rx: std::sync::Mutex::new(None),
            config_path_override: None,
            auto_installer_override: None,
            mismatch_interrupts: Arc::new(std::sync::Mutex::new(HashSet::new())),
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
    /// - Last MCP client disconnected (disconnect notify, count == 0) and the
    ///   count is still zero when the grace window expires (pulse-03): bridge
    ///   churn at session transitions is normal host behavior, so the exit is
    ///   debounced by [`DISCONNECT_GRACE`] — a connection arriving during the
    ///   window disarms it and the warm LSP fleet survives.
    /// - `catenary stop` received on the IPC socket (shutdown token) —
    ///   deliberate stops do not debounce.
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

        // Debounced last-client exit (pulse-03): when the disconnect census
        // hits zero this holds the deadline after which the daemon exits.
        // `None` = disarmed. The timer future is recreated from this absolute
        // deadline on every select pass, so the loop keeps accepting (and
        // serving IPC) while the window runs.
        let mut grace_deadline: Option<tokio::time::Instant> = None;

        loop {
            // Copied into the timer arm so the handler bodies below can
            // mutate the original without borrowing against its future.
            let deadline = grace_deadline;
            tokio::select! {
                result = self.mcp_listener.accept() => {
                    let (stream, _addr) = result.context("accept MCP connection")?;
                    if grace_deadline.take().is_some() {
                        self.grace_armed.store(false, Ordering::Release);
                        info!(
                            source = Source::DaemonLifecycle.as_str(),
                            "grace disarmed: client connected",
                        );
                    }
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
                    if self.connection_count.load(Ordering::Acquire) == 0
                        && grace_deadline.is_none()
                    {
                        grace_deadline =
                            Some(tokio::time::Instant::now() + self.disconnect_grace);
                        self.grace_armed.store(true, Ordering::Release);
                        info!(
                            source = Source::DaemonLifecycle.as_str(),
                            "last client disconnected — exit armed ({}s grace)",
                            self.disconnect_grace.as_secs(),
                        );
                    }
                }
                () = async move {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    grace_deadline = None;
                    self.grace_armed.store(false, Ordering::Release);
                    // Re-check the census at expiry — the authoritative test.
                    // A client that connected during the window (however it
                    // was counted) keeps the daemon alive.
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
        // Bridge↔daemon version-mismatch surfacing (ws41-02): the daemon-owned
        // snapshot writer (persistent surface) and the once-per-pairing interrupt
        // dedup set, both cloned into the bridge-hello callback below.
        let mismatch_snapshot = primary_session.as_ref().and_then(|s| s.snapshot.clone());
        let mismatch_interrupts = Arc::clone(&self.mismatch_interrupts);

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

                    // Bridge↔daemon version handshake (ws41-02): the bridge's
                    // hello reaches this callback with its reported `catenary-mcp`
                    // version (`None` for a pre-handshake bridge). Comparison,
                    // snapshot recording, and the once-per-pairing interrupt all
                    // live daemon-side in `handle_bridge_hello`.
                    mcp = mcp.on_bridge_hello(Box::new(move |bridge_version| {
                        handle_bridge_hello(
                            bridge_version,
                            mismatch_snapshot.as_ref(),
                            &mismatch_interrupts,
                        );
                    }));

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

    /// Shrinks the last-client-disconnect grace window (pulse-03, test-only).
    ///
    /// Production always debounces the last-client exit by
    /// [`DISCONNECT_GRACE`]; tests inject a small window so expiry-path
    /// tests run in milliseconds instead of a minute. No production caller.
    #[cfg(test)]
    #[must_use]
    const fn disconnect_grace_override(mut self, grace: Duration) -> Self {
        self.disconnect_grace = grace;
        self
    }

    /// Redirect `pin`/`unpin` config persistence to `path` (bug 109, test-only).
    ///
    /// Must be called **before** [`Self::with_session`], which reads the override
    /// into [`HookDispatchContext`]'s resolved config path. In-process router
    /// tests point this at a tempdir file so a `tool/roots-add` dispatched over
    /// the test's IPC socket persists there instead of the operator's real
    /// `~/.config/catenary/config.toml`. There is no production caller: the daemon
    /// leaves the override `None` and resolves [`user_config_path`].
    #[cfg(test)]
    #[must_use]
    fn config_path_override(mut self, path: PathBuf) -> Self {
        self.config_path_override = Some(path);
        self
    }

    /// Inject a stub-seamed background auto-installer (lsm 05, test-only).
    ///
    /// Must be called **before** [`Self::with_session`], which reads the
    /// override into [`HookDispatchContext`]. In-process router tests inject
    /// an installer whose runner/fetcher are stubs and whose managed home is a
    /// tempdir, so a `session-start/clear-editing` dispatch exercises the real
    /// detection→kick→background-task path without touching the network, the
    /// toolchain, or the operator's managed home. No production caller: the
    /// daemon leaves the override `None` and builds the real installer.
    #[cfg(test)]
    #[must_use]
    fn auto_installer_override(mut self, installer: crate::auto_install::AutoInstaller) -> Self {
        self.auto_installer_override = Some(installer);
        self
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

        // Env-seed registration (misc 192): register the daemon's
        // `CATENARY_ROOTS` boot roots as the `seed:env` tracker contributor so a
        // later re-sync that rebuilds the served union from tracker contributors
        // (a pin via `tool/roots-add`, an MCP disconnect) does not silently evict
        // the seed. Runs before `restore_pinned_roots` so the seed is already in
        // the union that the pin-restore re-sync pushes down. Zero-cost — the
        // session already serves these roots (its own boot `spawn_all`), so this
        // adds a tracker entry + roots-board line and spawns nothing.
        register_env_seed(&root_tracker, &session);

        // Persisted-pin restore (misc 175): re-add each `[roots] pinned` config
        // entry as a `hook` contributor so a pin survives a daemon restart. A
        // hand-edit to the array is honored the same way — adding a path IS a
        // pin, effective now. Restore is zero-cost: `add_roots` births a
        // config-loaded `Root` (a tracker entry + roots-board line) but nothing
        // is active on a fresh daemon, so `spawn_for_added_roots` spawns no
        // server until the root's first touch (the lazy path is preserved). A
        // missing path (deleted repo, unmounted volume) is left in the config and
        // NOT added to the tracker — the doctor missing-pin finding surfaces it;
        // Catenary never rewrites the user's config outside an explicit
        // pin/unpin, so a transiently absent mount stays pinned.
        restore_pinned_roots(&root_tracker, &session);

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

        // The background auto-installer (lsm 05): production builds the real
        // one against the daemon snapshot (its doctor/TUI-visible records); a
        // test injects a stub-seamed installer via `auto_installer_override`.
        let auto_installer = self
            .auto_installer_override
            .clone()
            .unwrap_or_else(|| crate::auto_install::AutoInstaller::new(session.snapshot.clone()));

        self.hook_ctx = Some(HookDispatchContext {
            sessions,
            primary: session,
            _logging: self.logging.clone(),
            root_tracker: Some(root_tracker),
            editing_guardrail: Arc::new(EditingGuardrail::new()),
            handoff: KeyedHandoff::new(),
            diag_rounds: DiagRoundRegistry::default(),
            worktree_watcher,
            ephemeral_mounts,
            first_sightings: FirstSightings::new(),
            promoted_prefixes: PromotedPrefixes::new(),
            project_config_nudges: ProjectConfigNudges::new(),
            auto_installer,
            worktree_registry,
            worktree_mounts: WorktreeMounts::new(),
            subagents,
            // Bug 109: production resolves the real user config; a test injects a
            // tempdir path via `config_path_override` so an in-process pin/unpin
            // can never reach the operator's `~/.config/catenary/config.toml`.
            user_config_path: self
                .config_path_override
                .clone()
                .unwrap_or_else(user_config_path),
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
        let registry = ctx.worktree_registry.clone();
        rt.spawn(async move {
            let mut ticker = tokio::time::interval(WORKTREE_ROOT_GC_INTERVAL);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                // Age out registry entries whose worktree dir is gone (bug 91) —
                // the crash-safe backstop for a stale tracked entry a
                // landed/removed worktree left behind, clearing its
                // once-per-worktree surfacing marks so a path recreated there
                // re-surfaces. Independent of the root reap below.
                let pruned = registry.prune_missing();
                if !pruned.is_empty() {
                    debug!(
                        source = Source::DaemonDispatch.as_str(),
                        count = pruned.len(),
                        "aged out worktree registrations whose dir is gone",
                    );
                }
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
        // The worktree kept-countdown reaper (root-ownership 04) rides this same
        // sweep — one activity model, one cadence. It needs the full dispatch
        // context to run `retire_root` (the MOUNT-only teardown discipline).
        let ctx = ctx.clone();
        rt.spawn(async move {
            let mut ticker = tokio::time::interval(EPHEMERAL_ROOT_SWEEP_INTERVAL);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;

                // Worktree kept countdown (root-ownership 04, the mount lifetime
                // for KEPT worktrees): retire every worktree MOUNT whose countdown
                // lapsed past `WORKTREE_KEPT_COUNTDOWN` untouched by a hook. Expiry
                // retires the mount ONLY (servers, root, lock — `retire_root`); it
                // NEVER deletes the dirty directory, which persists for `land`/`rm`.
                // The countdown is armed at subagent stop and reset by any hook
                // resolving into the worktree (the one hook seam). A LIVE worktree
                // (no countdown armed) is never a candidate.
                let expired_worktrees = ctx
                    .worktree_mounts
                    .expired_countdowns(Instant::now(), WORKTREE_KEPT_COUNTDOWN);
                for (contributor, root) in &expired_worktrees {
                    // Full retire discipline: every contributor declaring the root
                    // releases it, the watch + kept clock unregister, the lock and
                    // held-open docs go, the reduced union syncs once. The DIRECTORY
                    // is untouched (the maintainer-verbatim absolute).
                    retire_root(&ctx, &tracker, root).await;
                    info!(
                        source = Source::DaemonDispatch.as_str(),
                        session_id = worktree_contributor_session_id(contributor).unwrap_or(""),
                        contributor = %contributor,
                        worktree = %root.display(),
                        "expired kept-worktree mount countdown (mount retired; dirty dir left in place)",
                    );
                }
                if !expired_worktrees.is_empty() {
                    session.touch_snapshot();
                }
                // Durable-lock paid-idle countdown (root-ownership stage 2,
                // release leg 1): remove every lock dir that is paid (empty
                // ledger) and idle past the window. Rides this existing sweep —
                // same cadence as the ephemeral reaper. Indifferent to daemon
                // lifecycle: locks survive daemon churn and reboots, so this is
                // the only timer-driven release. A re-edit inside the window
                // re-arms the same lock with no new ceremony.
                let reaped_locks = crate::lock::reap_paid_idle_locks(
                    std::time::SystemTime::now(),
                    crate::lock::PAID_IDLE_TIMEOUT,
                );
                for encoded in &reaped_locks {
                    info!(
                        source = Source::DaemonDispatch.as_str(),
                        lock = %encoded,
                        "expired paid idle root lock",
                    );
                }
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

    /// Spawns the worktree-deletion reaper — the **release edge** for
    /// `worktree:*` roots (ticket 05; bug 106).
    ///
    /// Drains the channel the [`crate::worktree_watch::WorktreeWatcher`] feeds from
    /// its [`notify`] callback. Each [`crate::worktree_watch::WorktreeDeleted`] is
    /// the deletion of a watched worktree dir — for git subagents the host runs
    /// `git worktree remove` itself and fires no `WorktreeRemove` hook, so this is
    /// the prompt signal. Since a worktree root is now pinned-class (no idle
    /// expiry — bug 106), the vanished directory is the honest lifetime signal, so
    /// this watch is the primary teardown trigger, not merely a fast alternative
    /// to it.
    ///
    /// Each event retires the vanished root through the **full retire discipline**
    /// ([`retire_root`], misc 183's lesson): every contributor declaring the path
    /// releases exactly it (never just the `worktree:*` mount, which would orphan
    /// the per-root server set behind another holder), its deletion watch and idle
    /// clock unregister, its provenance leaves the ledger, and the reduced union
    /// syncs once. Reaping is idempotent: a double-reap from a coalesced delete
    /// burst, the GC, and the `SessionEnd` sweep is a harmless no-op
    /// (`retire_root` of an already-absent root changes nothing, and the re-sync is
    /// to the same union).
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
        let ctx = ctx.clone();
        rt.spawn(async move {
            while let Some(event) = rx.recv().await {
                // Retire the vanished root from ALL of its contributors, not just
                // the `worktree:*` mount (misc 183): `retire_root` unregisters the
                // watch + idle clock, prunes the provenance ledger, and re-syncs
                // once. Idempotent against a coalesced delete burst and the GC.
                retire_root(&ctx, &tracker, &event.worktree).await;
                // Scope the reap log under the contributing session — the same
                // firehose shard as the mount (which logs inside the
                // session-scoped hook handler) — so a worktree's full lifecycle
                // co-locates. The reaper has no session span, so recover the id
                // from the contributor; `session_id = ""` falls through to the
                // daemon scope for a malformed key (no behavior change).
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    session_id =
                        worktree_contributor_session_id(&event.contributor).unwrap_or(""),
                    contributor = %event.contributor,
                    worktree = %event.worktree.display(),
                    "retired worktree root on dir deletion",
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
    /// Each resolution's manifest is installed as the process-wide
    /// [`crate::recipes::active_manifest`] (diagnostics-debt 04b), so the LSP
    /// construction seams project a re-pin's discipline/casing/classification
    /// without a binary release. The loader has already degraded a fetch failure,
    /// a bad signature, or an unreadable schema down to the seed before this point,
    /// so the installed manifest is never *less* verified than the offline floor
    /// (directional safety — an absent fetch stays seed-only, never more trusting).
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
                // Thread the resolved manifest into the LSP construction seams
                // (diagnostics-debt 04b): a re-pin's discipline/casing/classification
                // reaches live daemons without a binary release. The loader already
                // floored a failed/unreadable resolution at the seed, so this only
                // ever installs an equal-or-more-verified manifest.
                crate::recipes::install_active_manifest(std::sync::Arc::new(
                    resolved.payload.manifest.clone(),
                ));
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

/// Handles a bridge hello daemon-side: compare, record, interrupt (ws41-02).
///
/// `bridge` is the connecting bridge's reported `catenary-mcp` version (`None`
/// for a pre-handshake bridge that carried no version); the daemon compares it
/// against the version it links ([`catenary_mcp::version`]) via
/// [`catenary_mcp::version_mismatch`], so the comparison is daemon-side and
/// direction-blind — a pre-handshake bridge reads as mismatched precisely
/// because the daemon, not the bridge, decides.
///
/// On agreement it clears any prior mismatch record (a `/mcp` restart or a
/// daemon bounce that heals the pairing silences every surface). On
/// disagreement it:
///
/// 1. records the mismatch onto the snapshot so the persistent surfaces
///    (`catenary doctor`, the TUI board, the `SessionStart` hook line) carry the
///    reminder until the versions agree, and
/// 2. fires the ONE error-tier desktop interrupt the pairing earns — deduped on
///    the `(bridge, daemon)` pairing key so repeated session-starts reporting the
///    same pair never re-fire (a per-session-start refire would pollute both the
///    desktop and the TUI health surface).
///
/// The interrupt is a `tracing::error!()` — in this codebase that IS the desktop
/// interrupt (the `LoggingServer` routes error severity to the notification
/// sink) and also lands as a TUI health finding; the dedup keeps it to one.
#[cfg(unix)]
fn handle_bridge_hello(
    bridge: Option<&str>,
    snapshot: Option<&Arc<crate::state_snapshot::SnapshotWriter>>,
    dedup: &Arc<std::sync::Mutex<HashSet<String>>>,
) {
    let daemon = catenary_mcp::version();

    // Record (or clear) the persistent surface first, so the doctor/board
    // finding and the SessionStart line are already true when the interrupt
    // fires — the interrupt is the alert; the record is the standing reminder.
    if let Some(writer) = snapshot {
        writer.record_bridge_mismatch(bridge, daemon);
    }

    let Some(mismatch) = catenary_mcp::version_mismatch(bridge, daemon) else {
        // Versions agree — nothing to surface.
        return;
    };

    // One interrupt per observed pairing this daemon lifetime.
    let key = mismatch.pairing_key();
    let first_time = {
        let mut fired = dedup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fired.insert(key)
    };
    if !first_time {
        return;
    }

    error!(
        source = Source::DaemonLifecycle.as_str(),
        bridge_version = mismatch.bridge_label(),
        daemon_version = mismatch.daemon_version(),
        "Catenary bridge↔daemon version mismatch: {}",
        mismatch.message(),
    );
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

/// Resolves the `(contributor, worktree)` a `SubagentStop` should reap by cwd —
/// pure path algebra, no identity (root-ownership 04, AUDIT #10/#11).
///
/// Resolves the *enclosing* worktree root of `cwd`
/// ([`crate::companions::enclosing_worktree_root`]) and keys on THAT canonical
/// path — never an exact match on the raw cwd, so a final `cd` into a
/// subdirectory of the worktree still reaps. Contributors are uniformly
/// path-shaped (`worktree:{session}:{canonical-path}`), so the same path that
/// mounted the worktree resolves its teardown key; the identity-first registry
/// lookup (`path_for_identity`) and the `worktree:{session}:{agent}` contributor
/// namespace retired with the cwd-activity mechanism.
///
/// Returns `None` when `cwd` resolves to no enclosing worktree.
#[cfg(unix)]
fn resolve_stop_reap_target(session_id: &str, cwd: Option<&str>) -> Option<(String, PathBuf)> {
    cwd.and_then(|c| {
        crate::companions::enclosing_worktree_root(Path::new(c)).map(|root| {
            let root = root.canonicalize().unwrap_or(root);
            (format!("worktree:{session_id}:{}", root.display()), root)
        })
    })
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

/// Handle the `pre-tool/claim` hook stage (root-ownership stage 2).
///
/// The `PreToolUse` hook for `catenary claim <root>` calls this with the
/// claimant's identity (`format`+`session_id`+`agent_id` — the one seam identity
/// appears) and the target `root`. The daemon:
/// 1. Runs the mechanical guard: refuse while a diagnose round is in flight on
///    any identity (an activity fact — a diagnosing agent is demonstrably
///    present, not gone). The registry keys by editing identity, not root, and a
///    round may diagnose a batch spanning roots, so the conservative "any in
///    flight" signal is the honest one.
/// 2. Performs the one atomic owner-file rename to the claimant's tuple
///    ([`crate::lock::claim`]); the old→new title pair is the audit record.
/// 3. Records the takeover loudly: a firehose event and a `warn!`-level TUI
///    finding (the human sees every takeover — `warn!` is the finding tier; an
///    `error!` would fire an unwanted desktop interrupt).
/// 4. Stages the rendered answer for the identity-less CLI to drain.
///
/// Returns `{"status":"staged"}` on a completed takeover, `{"status":"refused",
/// "message":…}` when the guard blocks it (a diagnose round in flight), or
/// `{"status":"unlocked"}` / `{"status":"already_ours"}` when there is nothing to
/// take. The hook maps a non-`staged` outcome to its own degrade path.
#[cfg(unix)]
async fn handle_claim_stage(
    ctx: &HookDispatchContext,
    raw: &serde_json::Value,
) -> serde_json::Value {
    let Some(root_str) = raw.get("root").and_then(|v| v.as_str()) else {
        return serde_json::json!({"status": "error", "message": "missing root"});
    };
    let root = PathBuf::from(root_str);
    let root = root.canonicalize().unwrap_or(root);

    // Mechanical guard: refuse while a diagnose round is executing (an activity
    // fact, not a timeout). The lock is a hook-plane fact, so if the guard says
    // "busy" the takeover waits for a genuinely-absent editor.
    if ctx.diag_rounds.any_in_flight() {
        return serde_json::json!({
            "status": "refused",
            "message": format!(
                "a diagnose round is in flight — {} is actively being diagnosed; \
                 try `catenary claim` again once it settles",
                root.display()
            ),
        });
    }

    let claimant = crate::lock::Owner::new(
        raw.get("format")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        raw.get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        raw.get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    );
    let now = std::time::SystemTime::now();

    let answer = match crate::lock::claim(&root, &claimant, now) {
        crate::lock::Claimed::Ok {
            previous_age,
            due,
            paid_and_idle,
        } => crate::lock::claim_answer(&root, previous_age, due, paid_and_idle),
        crate::lock::Claimed::Unlocked => {
            return serde_json::json!({"status": "unlocked"});
        }
        crate::lock::Claimed::AlreadyOurs => {
            return serde_json::json!({"status": "already_ours"});
        }
    };

    // Record the takeover loudly: a firehose event AND a warn-level TUI finding
    // (the human sees every takeover). `warn!` is the finding tier in this
    // codebase — NOT `error!`, which fires a desktop interrupt a routine
    // hand-over does not earn.
    warn!(
        source = Source::DaemonDispatch.as_str(),
        root = %root.display(),
        claimant = %claimant.file_name(),
        "root claimed from a prior editor",
    );

    // Stage the rendered answer for the CLI to drain. Acquiring the per-key
    // permit blocks only behind another in-flight *claim* stage (never
    // daemon-wide) and holds for milliseconds. A permit-acquire failure is
    // non-fatal: report `staged` anyway so the rename (already done) is not
    // reversed, and the CLI's degrade path reads the lock state.
    if let Ok(permit) = ctx.handoff.acquire(HandoffKey::Claim).await {
        ctx.handoff.stage(
            HandoffKey::Claim,
            HandoffContext {
                payload: HandoffPayload::Claim {
                    answer: answer.clone(),
                },
                permit,
            },
        );
    }

    serde_json::json!({"status": "staged", "answer": answer})
}

/// Retires a landed/removed worktree root from ALL daemon-side state in the
/// same round-trip the disposal ran (bug 93).
///
/// The land verb's contract is "landed and gone" — but before this leg the
/// daemon-side root could live on: [`reap_worktree_root`] only drops the one
/// `worktree:{sid}:{path}` mount contributor, so a root also held by an
/// `ephemeral:`/`hook`/`mcp:` contributor stayed in the union, its per-root
/// language servers stayed up, and a later spawn against the now-deleted `cwd`
/// died instantly as `initialize failed` — routed by the removed path.
///
/// This retires the root completely: every contributor declaring it releases
/// exactly this root (`remove_root`, never `remove_contributor` — a multi-root
/// contributor such as an `mcp:` connection declaring the primary repo keeps
/// its other roots; a per-path contributor empties and drops), its deletion
/// watch and idle clock are unregistered, and its language-activity provenance
/// leaves the snapshot ledger — so the doctor and TUI stop rendering `routed
/// by … in <removed root>`. The reduced root set is synced once, which shuts
/// down the root's per-root instances and drops their board entries.
/// Idempotent: removing an absent root is a no-op, so this composes with the
/// watch reaper, the GC, and `SessionEnd`.
///
/// Internal lifecycle only: `debug!`, never `warn!`/`error!` — a retired root is
/// expected convergence, not an actionable condition (`docs/src/tracing-conventions`).
#[cfg(unix)]
async fn retire_root(ctx: &HookDispatchContext, tracker: &RootTracker, worktree: &Path) {
    let contributors = tracker.contributors_of_root(worktree);
    for contributor in &contributors {
        tracker.remove_root(contributor, worktree);
        // Drop the in-memory deletion watch + idle clock keyed on this
        // contributor so neither outlives the retired root (idempotent; a
        // multi-root contributor has neither registered, so both are no-ops).
        if let Some(ref watcher) = ctx.worktree_watcher {
            watcher.unregister(contributor);
        }
        ctx.worktree_mounts.remove(contributor);
    }
    // Prune the provenance ledger so no phantom `routed by … in <path>` survives
    // the removal — even when no contributor held the path (a stale ledger entry
    // from a prior touch), so this runs unconditionally.
    ctx.primary.forget_root_activity(worktree);
    // Root retirement takes the durable lock and its ledger with the kitchen
    // (root-ownership stage 2, release leg 2). The worktree encoding must match
    // the acquisition-time encoding, which canonicalizes; retire both the raw
    // and canonical spellings so a symlinked-prefix worktree still clears.
    crate::lock::retire(worktree);
    if let Ok(canonical) = worktree.canonicalize()
        && canonical != worktree
    {
        crate::lock::retire(&canonical);
    }
    // Close the root's held-open documents (root-ownership stage 3): a diagnose
    // round tags its documents with the root, so root retirement is their
    // teardown edge — the identity-keyed Stop close retired with the handoff.
    // Both spellings, matching the ledger retire above.
    ctx.primary.close_root_docs(worktree).await;
    if let Ok(canonical) = worktree.canonicalize()
        && canonical != worktree
    {
        ctx.primary.close_root_docs(&canonical).await;
    }
    let global = tracker.global_roots_rich();
    if let Err(e) = ctx.primary.sync_roots(global).await {
        debug!(
            source = Source::DaemonDispatch.as_str(),
            worktree = %worktree.display(),
            "root sync after worktree retirement failed: {e}",
        );
    }
    debug!(
        source = Source::DaemonDispatch.as_str(),
        worktree = %worktree.display(),
        contributors = contributors.len(),
        "retired landed/removed worktree root from the daemon",
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

/// Handle a `tool/worktree-rm` request (misc 151/166): load the sidecar, reap any
/// live mount, and dispose class-appropriately, returning the CLI response.
///
/// An agent worktree removes on the caller's captured-work assertion (the
/// force-shaped landing path — firehose-logged); a feats worktree refuses dirty
/// (uncommitted or unpushed). `--force` (misc 166) is the explicit, user-typed
/// exception to the never-auto-clean rule: it routes *any* class through the
/// force-shaped disposal path ([`crate::worktree_dispose::remove_agent_asserted`])
/// — the same one that retires the root and sweeps the sidecar — so a superseded
/// dirty worktree is discarded properly instead of via a raw-git dance. The
/// response then names the discarded work (the dirty summary). A path with no
/// sidecar is never ours to touch.
#[cfg(unix)]
async fn handle_worktree_rm(
    ctx: &HookDispatchContext,
    raw: &serde_json::Value,
) -> serde_json::Value {
    let Some(raw_path) = raw.get("path").and_then(|v| v.as_str()) else {
        return serde_json::json!({ "status": "error", "message": "missing path" });
    };
    let force = raw
        .get("force")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
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

    // Capture the dirty summary BEFORE removal so the response can name what a
    // forced discard drops — the directory is gone once disposal succeeds.
    let discarded = force
        .then(|| crate::worktree_dispose::dirty_summary(&meta))
        .flatten();

    let disposition = if force {
        // The explicit, user-typed exception to the never-auto-clean rule: discard
        // through the force-shaped disposal path regardless of class or dirty state
        // — it retires the root and sweeps the sidecar, so no raw-git dance leaves
        // the registry inconsistent. Firehose-logged as the deliberate exception.
        if let Some(summary) = &discarded {
            info!(
                source = Source::DaemonDispatch.as_str(),
                worktree = %worktree.display(),
                discarded = %summary,
                "worktree rm --force: discarding a dirty worktree on the caller's explicit assertion",
            );
        } else {
            info!(
                source = Source::DaemonDispatch.as_str(),
                worktree = %worktree.display(),
                "worktree rm --force: removing a clean worktree",
            );
        }
        crate::worktree_dispose::remove_agent_asserted(&meta)
    } else if meta.class == crate::worktree_create::WORKTREE_CLASS_FEAT {
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

    // Retire the root in the SAME round-trip that removed the directory (bug 93):
    // once the dir is gone the root can route nothing, so every contributor lets
    // go, its per-root servers shut down, and its provenance leaves the ledger —
    // no window where a removed directory is still a live root. A kept/refused
    // disposition left the dir in place, so the root stays.
    if disposition.is_disposed()
        && let Some(tracker) = &ctx.root_tracker
    {
        retire_root(ctx, tracker, &worktree).await;
    }

    worktree_rm_response(&disposition, &worktree, discarded.as_deref())
}

/// Map a disposal [`Disposition`](crate::worktree_dispose::Disposition) to the
/// `catenary worktree rm` CLI response.
///
/// `discarded` is `Some(summary)` only for a forced discard of a dirty worktree
/// (misc 166) — it rides back so the CLI can name what was dropped.
#[cfg(unix)]
fn worktree_rm_response(
    disposition: &crate::worktree_dispose::Disposition,
    worktree: &Path,
    discarded: Option<&str>,
) -> serde_json::Value {
    use crate::worktree_dispose::Disposition;
    match disposition {
        Disposition::Disposed | Disposition::Remnant => serde_json::json!({
            "status": "ok",
            "removed": true,
            "path": worktree.display().to_string(),
            "discarded": discarded,
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
/// the registry entry is dropped (and the dir removal fires the vanish-watch,
/// which retires the mount); on a dirty keep the worktree is left untouched and
/// the once-per-path firehose record fires ([`surface_dirty_kept`]) unless the
/// removal was host-initiated (`WorktreeRemove` logs the divergence inside
/// [`crate::worktree_dispose::dispose`] instead). The kept dir persists for
/// `land`/`rm`, discoverable via the session-start linger nag and `worktree ls`;
/// its mount is governed by the kept countdown, never auto-cleaned (the
/// maintainer-verbatim absolute). A path with no registry entry and no sidecar is
/// silently skipped (never ours).
///
/// `stopping_agent_id` is the ownership gate for the `SubagentStop` path (bug
/// 103). The stop's cwd is resolved to its *enclosing* worktree root, so a
/// nested/foreign subagent whose cwd merely sits inside a live sibling's worktree
/// resolves onto that sibling — and the clean arm would then **delete** the live
/// owner's tree. Passing `Some(stopping_agent_id)` gates the dispose to a matching
/// owner: the tree is disposed only when the stopping agent IS the worktree's
/// owner (its dirname, bug 91's [`worktree_owner_label`]). A foreign stop skips
/// the dispose entirely; the hourly GC (which prunes gone dirs, `497c989`) carries
/// any genuinely-orphaned tree. Pass `None` to bypass the gate for a
/// host-initiated `WorktreeRemove`, where the human asked for removal of this
/// exact path and there is no stopping-agent identity.
#[cfg(unix)]
fn dispose_worktree_in_background(
    registry: &WorktreeRegistry,
    session_id: &str,
    stopping_agent_id: Option<&str>,
    worktree: &Path,
    host_initiated: bool,
) {
    let Some(meta) = registry
        .get(worktree)
        .or_else(|| load_meta_from_sidecar(worktree))
    else {
        return;
    };
    // Bug 103 ownership gate: a `SubagentStop` may dispose only its own worktree.
    // A stop whose agent identity does not own this tree must not select it for
    // disposal, even though the cwd resolution enclosed it. `None` (a
    // host-initiated `WorktreeRemove`) bypasses the gate.
    if let Some(stopping_agent_id) = stopping_agent_id
        && !stop_owns_worktree(stopping_agent_id, worktree)
    {
        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            stopping_agent_id = stopping_agent_id,
            owner = %worktree_owner_label(worktree),
            worktree = %worktree.display(),
            "subagent stop dispose skipped — stopping agent is not the worktree owner \
             (cwd resolution enclosed a live sibling; not disposing)",
        );
        return;
    }
    let disposition = crate::worktree_dispose::dispose(&meta, host_initiated);
    match &disposition {
        crate::worktree_dispose::Disposition::Disposed
        | crate::worktree_dispose::Disposition::Remnant => registry.forget(worktree),
        crate::worktree_dispose::Disposition::KeptDirty { .. } if !host_initiated => {
            surface_dirty_kept(registry, session_id, worktree);
        }
        _ => {}
    }
}

/// The owning agent id of a worktree, taken from its on-disk directory name (bug
/// 91).
///
/// A subagent worktree's leaf directory name **is** the agent id (misc 150 —
/// `worktree_segment` uses the bare `agent-<id>` id as the path segment), so the
/// dir name is the correct, self-describing owner — never the id of whatever
/// subagent happened to trigger the dispose (its cwd may merely *enclose* this
/// worktree). Falls back to the full path display if the leaf is unreadable.
#[cfg(unix)]
fn worktree_owner_label(worktree: &Path) -> String {
    worktree.file_name().map_or_else(
        || worktree.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Whether the agent stopping at `SubagentStop` **owns** `worktree` — the bug 103
/// dispose gate.
///
/// `resolve_stop_reap_target` resolves a stop's target by cwd — the *enclosing*
/// worktree root — so a nested/foreign subagent whose cwd merely sits inside a
/// live sibling's tree resolves onto that sibling. Disposing there would delete a
/// live owner's workspace (bug 103). Ownership is the tree's on-disk directory
/// name — bug 91's dirname-IS-owner primitive ([`worktree_owner_label`]): a
/// subagent worktree's leaf segment is exactly its agent id (misc 150). Only a
/// stop whose agent identity equals that owner may dispose; a foreign stop must
/// not select the tree, letting the hourly GC carry any genuinely-orphaned one.
/// (Identity appears here only as the ownership tag at the hook, never as a
/// daemon lookup key — the same seam the lock uses.)
#[cfg(unix)]
fn stop_owns_worktree(stopping_agent_id: &str, worktree: &Path) -> bool {
    // An empty id (no agent identity in the stop payload) can never prove
    // ownership; decline rather than fall through to a cwd-selected delete.
    !stopping_agent_id.is_empty() && stopping_agent_id == worktree_owner_label(worktree)
}

/// Record a dirty worktree kept at `SubagentStop` — once per worktree path (bug
/// 91), a firehose/TUI record only (root-ownership 04).
///
/// The identity-addressed parent-agent `additionalContext` side channel (the
/// `ParentContextQueue`) retired with the worktree countdown: nothing needs to
/// find "the agent that spawned it." The dirty worktree is discoverable exactly
/// where it lives — the session-start linger nag and `catenary worktree ls` — and
/// its mount is governed by the kept countdown. The `warn!` stays as the
/// firehose/log record of the event (queryable via `catenary query`, a TUI health
/// finding).
///
/// Bug 91: the owner is the worktree's own directory name
/// ([`worktree_owner_label`]), not the triggering subagent's id, and the record
/// is deduped to **once per worktree path** ([`WorktreeRegistry::mark_surfaced`]).
/// The dedup never suppresses a *first* report — a dirty worktree that exists is
/// always recorded.
#[cfg(unix)]
fn surface_dirty_kept(registry: &WorktreeRegistry, session_id: &str, worktree: &Path) {
    if !registry.mark_surfaced(worktree) {
        return; // already surfaced this worktree — one record per path
    }
    let owner = worktree_owner_label(worktree);
    warn!(
        source = Source::DaemonDispatch.as_str(),
        session_id = %session_id,
        "subagent `{owner}` left a dirty worktree at `{}` (kept; land its work \
         or `catenary worktree rm` it)",
        worktree.display(),
    );
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

/// Whether a `post-agent/require-release` dispatch is the MAIN agent's
/// top-level `Stop` (wf-04): `hook_event_name == "Stop"` **and** no agent id on
/// the request.
///
/// In Claude Code the main agent is the identity WITHOUT an `agentId` (a
/// display label, never a key); any identity WITH one is a subagent and never
/// draws the merged-linger nag. `SubagentStop` is excluded outright by the
/// event-name leg.
#[cfg(unix)]
fn is_main_agent_stop(raw: &serde_json::Value) -> bool {
    raw.get("host_payload")
        .and_then(|hp| hp.get("hook_event_name"))
        .and_then(serde_json::Value::as_str)
        == Some("Stop")
        && raw
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .is_empty()
}

/// The merged-linger advisory for the main agent's Stop (wf-04), or `None`
/// when nothing qualifies.
///
/// Second linger oracle beside [`lingering_worktree_nag`]'s owner-dead/
/// root-unmounted one: `git cherry` patch-equivalence
/// ([`crate::worktree_dispose::is_squash_merged`]) — a squash landing creates
/// no ancestry, so a landed worktree lingers until someone runs
/// `catenary worktree rm`. Collects the session's registered worktrees
/// satisfying ALL of: dir present, branch already squash-merged, and not yet
/// nagged this daemon lifetime. The once-mark is the SAME `nagged` ledger the
/// existing linger nag uses, and this runs AFTER it in the dispatch, so a
/// worktree qualifying under both oracles draws exactly one line (the dedupe)
/// and every worktree is nagged once per daemon lifetime regardless of oracle.
///
/// Advisory, never a gate: the caller rides this on the `merged_nudge`
/// response side-channel (surfaced as a Claude `systemMessage`), and the
/// require-release outcome — allow or block — is untouched. The oracle's
/// amended-landing residual (patch-equivalence defeated → no line here; the
/// worktree stays visible through the general linger surfacing) is documented
/// on `is_squash_merged`.
#[cfg(unix)]
fn merged_worktree_nudge(ctx: &HookDispatchContext, session_id: &str) -> Option<String> {
    let mut merged = Vec::new();
    for meta in ctx.worktree_registry.metas_for_session(session_id) {
        if !meta.worktree.exists() {
            continue; // dir gone — nothing left to rm
        }
        if !crate::worktree_dispose::is_squash_merged(&meta) {
            continue; // unmerged, amended landing, or oracle silence — no line
        }
        if !ctx.worktree_registry.mark_nagged(&meta.worktree) {
            continue; // already nagged this daemon lifetime (by either oracle)
        }
        merged.push(meta.worktree.clone());
    }
    if merged.is_empty() {
        return None;
    }
    Some(merged_nudge_message(&merged))
}

/// Render the merged-linger advisory line (wf-04) for the given worktrees.
#[cfg(unix)]
fn merged_nudge_message(merged: &[PathBuf]) -> String {
    let list = merged
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let count = merged.len();
    if count == 1 {
        format!("1 worktree is already merged into main; `catenary worktree rm` it:\n{list}")
    } else {
        format!(
            "{count} worktrees are already merged into main; `catenary worktree rm` them:\n{list}"
        )
    }
}

/// Races `pipeline` against client disconnect (bug 24): resolves `Some(outcome)`
/// when the pipeline completes first, `None` when the client's read half
/// completes first (EOF, error, or any stray byte — a well-behaved client
/// sends nothing after its request). The losing pipeline future is dropped.
#[cfg(unix)]
async fn race_against_disconnect<T>(
    pipeline: impl std::future::Future<Output = T>,
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Option<T> {
    use tokio::io::AsyncReadExt;

    let mut probe = [0u8; 1];
    tokio::select! {
        outcome = pipeline => Some(outcome),
        _ = reader.read(&mut probe) => None,
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

    // ── The one hook seam: cwd-activity resets every mount lifetime (root-ownership 04) ──
    //
    // Every hook carries cwd, so ANY hook resolving into a mounted root is
    // activity — one activity model for every mount lifetime, fed from this one
    // seam. It refreshes the covering ephemeral root's idle clock AND the covering
    // worktree's kept countdown (clearing any blocked-on-permission flag too), so
    // an ephemeral or kept-worktree mount under active hook traffic — edits,
    // reads, even a PermissionRequest — never idle-expires. Reads count: a bare
    // `catenary grep`/`glob` is not a hook, but the host's own PreToolUse for the
    // read tool is, and it lands here. Pure refresh — mounting is a query's job
    // (`ensure_ephemeral_mounts`), never a hook's, so this stays fast. Runs before
    // every method short-circuit so no hook is exempt.
    if let Some(cwd) = hook_cwd(&raw) {
        let canonical = Path::new(cwd)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(cwd));
        let now = Instant::now();
        ctx.ephemeral_mounts.touch_covering(&canonical, now);
        ctx.worktree_mounts.touch_covering(&canonical, now);
    }

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
    // guardrail, session registry, and roots.
    //
    // Short-circuits before get_or_create_router to avoid creating
    // a new session just to immediately clean it up.
    if method == "session-end/cleanup" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        // Release editing guardrail locks (idempotent if MCP
        // disconnect already ran).
        ctx.editing_guardrail.release_all(&session_id);

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
                // Uniformly path-shaped (`worktree:{session}:{canonical-path}`):
                // the teardown routes (SubagentStop, WorktreeRemove) rebuild the
                // exact key by canonicalizing the same worktree path, so no
                // identity is needed to find the mount. The identity-shaped
                // `worktree:{session}:{agent}` form retired with the registry
                // identity lookups (root-ownership 04, AUDIT #10/#11).
                let (_canonical, contributor) = worktree_contributor(&session_id, &worktree);
                tracker.set_roots(&contributor, vec![worktree.clone()]);
                // Track the mounted worktree root (root-ownership 04): a LIVE
                // worktree is pinned-class (no countdown) — its release edge is
                // the vanish-watch — until its subagent stops dirty, when the
                // SubagentStop path arms the kept countdown on this mount.
                ctx.worktree_mounts.track(&contributor, &worktree);

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
            // Mounts are uniformly path-keyed (`worktree:{sid}:{canonical-path}`;
            // root-ownership 04), so `path_key` rebuilt from the canonicalized
            // `worktree_path` IS the mount key. The reverse-lookup by tracked-root
            // VALUE stays as a belt-and-suspenders match for any legacy/foreign
            // key spelling, falling back to the path form.
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
            let sid = session_id.clone();
            let wt = canonical.clone();
            tokio::task::spawn_blocking(move || {
                // Host-initiated removal: no stopping-agent identity to gate on —
                // the human asked for this exact path (bug 103 gate bypassed).
                dispose_worktree_in_background(&registry, &sid, None, &wt, true);
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
    // returns no decision). Marks the worktree root enclosing the prompt's cwd
    // **blocked** — a subagent parked at a permission prompt — so `catenary
    // worktree ls` renders its root-state as `blocked` (misc 151). Resolved by
    // PATH (the prompt's cwd → its enclosing worktree), not identity: the
    // identity-scoped mark and the no-agent-id coarse session fallback retired
    // with the registry identity lookups (root-ownership 04, AUDIT #11). The flag
    // clears on any subsequent activity resolving into the root (`touch_covering`
    // at the one hook seam). It gates no lifetime decision — the kept countdown
    // is pure hook-activity reset, with no pause machinery (the answer-desk
    // ruling); a PermissionRequest is itself activity that resets the countdown
    // via the one hook seam.
    //
    // Short-circuits before get_or_create_router: daemon-level root concern only.
    if method == "permission-request/blocked" {
        let scope_id = uuid::Uuid::new_v4().to_string();

        if let Some(cwd) = hook_cwd(&raw) {
            let canonical = Path::new(cwd)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(cwd));
            let marked = ctx.worktree_mounts.mark_blocked_covering(&canonical);
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                cwd = cwd,
                count = marked,
                "permission prompt: marked the enclosing worktree root(s) blocked",
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

        // ── The answer desk (misc 201, decision 031) ──────────────────
        //
        // Compute the read-class permission decision from the forwarded payload:
        // sensitive → deny with teaching; in declared scope → quiet allow (pin
        // the realpath); `always_read` → quiet allow + first-time session
        // promotion; out of scope → LOUD allow (allow + record). A write-class
        // tool or an unresolvable target yields no decision — the human's prompt
        // stands (fail-PASS). Emitted as the response body so the hook CLI prints
        // it verbatim; the daemon being unreachable already fails PASS CLI-side.
        let response_body = raw
            .get("host_payload")
            .and_then(|hp| {
                resolve_permission_decision(
                    hp,
                    ctx.root_tracker.as_ref(),
                    &ctx.primary.config,
                    &ctx.promoted_prefixes,
                    &session_id,
                )
            })
            .map(|(decision, tool_name)| {
                record_loud_read(&decision, &session_id);
                decision
                    .to_hook_json(permission_input_key(&tool_name))
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        emit_hook_event(
            tracing::Level::DEBUG,
            &session_id,
            &method,
            Some(&scope_id),
            &response_body,
            "outgoing hook response",
        );

        writer.write_all(response_body.as_bytes()).await?;
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

    // ── Hit-batch annotation stream (ws43) ──────────────────────
    //
    // `tool/hitstream` is opened by `catenary grep` (ws43-02) and `catenary
    // glob` (ws43-03) — the ONLY search surface the daemon serves (the
    // `tool/grep` and `tool/glob` executor arms retired). The method line has
    // already been read; the CLI now streams `HitFrame` batches on this same
    // connection. The daemon annotates each batch under budget with the REAL
    // enrichment (the executors' LSP enrichment, migrated into
    // [`crate::bridge::HitstreamEnricher`]) at the batch's requested weight —
    // grep anchors for a weight-less batch, listing/outline bodies for a
    // weighted one — and streams `AnnotationFrame` batches back, preserving
    // batch order. The per-batch WS31 nudge and the query auto-mount (with its
    // ws43-05 sensitive-path gate) ride each annotation call, keyed on the
    // canonical hit paths the batches carry; the walk-level observation set on
    // the `End` terminator feeds the executor's once-per-walk nudge/reap rule
    // (`observe_walk` — grep only; glob ships no reap scopes, a scoped walk
    // never proves absence). A malformed frame or a socket fault tears the
    // connection down; the CLI, seeing an incomplete annotation stream,
    // completes its results unannotated in place — the same output as
    // daemon-absent (degrade-only). This arm is a native async citizen: read
    // batch → await (budgeted) → write batch, no lock guard held across an
    // await.
    if method == METHOD_HITSTREAM {
        let annotator = HitstreamAnnotator {
            ctx: &ctx,
            inner: ctx.primary.hitstream_enricher(),
        };
        crate::hitstream::annotate_connection(
            &mut buf_reader,
            &mut writer,
            &annotator,
            crate::hitstream::ANNOTATION_BATCH_BUDGET,
        )
        .await?;
        return Ok(());
    }

    // ── Diagnostics serve: the ledger is the batch ───────────────
    //
    // `tool/editing-stop` is sent by the `catenary diagnostics` CLI command
    // (internal method name unchanged). Root-ownership stage 3 retired the
    // two-phase identity handoff (`pre-tool/editing-stop` prepare + staged
    // snapshot): the daemon now serves diagnoses against the on-disk lock
    // ledger — the durable touch-tree the edit seam booked — so the batch a bare
    // run computes over IS the due set read from disk (`crate::lock::due_files`),
    // enumerated over the caller's kitchens by pure path algebra — the cwd's
    // root plus every same-owner debtor root (`crate::lock::bare_serve_roots`,
    // bug 121; no identity below the hook). There is no mirror and therefore no
    // drift.
    if method == "tool/editing-stop" {
        // Scoped paths from the request. The CLI resolves relative paths against
        // its cwd before dispatch, so these are absolute. The bare form sends an
        // empty set → the whole ledger due set (the batch) is diagnosed; a
        // non-empty set names files to diagnose on demand (the pull-anything
        // form — served regardless of debt).
        //
        // Canonicalize each named path at this ingestion seam, once (misc 193,
        // the grep/glob/file-accumulation rule): fs roots are canonical, but the
        // caller passes its raw spelling. On a symlinked-prefix host (macOS
        // `$TMPDIR` → `/private/var/…`) the raw spelling misses the canonical
        // roots inside `ensure_clients_for_paths`, so the serve never ensures the
        // spawn and a cold registry reads as `[no LSP coverage]` (the `ee12779`
        // macOS CI red). A nonexistent path keeps its spelling — the pipeline's
        // out-of-scope classifier names it `missing` as before.
        let scoped_files: Vec<PathBuf> = raw
            .get("files")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .map(|p| p.canonicalize().unwrap_or(p))
                    .collect()
            })
            .unwrap_or_default();
        let scoped = !scoped_files.is_empty();

        // The caller's cwd (root-ownership stage 3): the bare form resolves its
        // due set by pure path algebra — cwd → enclosing root → ledger. The CLI
        // sends its cwd; an absent cwd falls back to the daemon's (degrades the
        // bare form to "no root here → no debt", never a false serve).
        let caller_cwd = raw.get("cwd").and_then(|v| v.as_str()).map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );

        // Resolve the kitchens the bare form pays (bug 121): the enclosing lock
        // root of the caller's cwd PLUS every other root whose ledger holds
        // debt attributable to the same owner — the edit seam books each file
        // into its OWN resolved root, so a session's debt can span kitchens the
        // caller is not standing in (`crate::lock::bare_serve_roots` documents
        // the attribution policy). A scoped run names its own files and needs
        // no root resolution for the diagnose set (delivery groups by each
        // file's root).
        let bare_roots: Vec<PathBuf> = if scoped {
            Vec::new()
        } else {
            crate::lock::bare_serve_roots(&caller_cwd)
        };

        // The diagnose set:
        // - scoped: exactly the named paths (served regardless of debt);
        // - bare:   the union of the served roots' ledger due sets
        //           (`crate::lock::due_files`), the single source of truth — no
        //           in-memory mirror. Ledgers are disjoint (a file books into
        //           its innermost root), and the receipt groups per root, so
        //           the concatenation renders one section per kitchen.
        let diag_files: Vec<PathBuf> = if scoped {
            scoped_files.clone()
        } else {
            bare_roots
                .iter()
                .flat_map(|root| crate::lock::due_files(root))
                .collect()
        };

        // The held-open document owner (root-ownership stage 3): the ROOT, not an
        // identity key. Documents a diagnose round opens are tagged with their
        // root, so root retirement / the paid-idle reap closes them — no identity
        // below the hook. A scoped run tags each file's own root; the bare run
        // tags its first served kitchen (the cwd's root when resolvable).
        // `process_files_batched` takes one owner for the whole round, so a run
        // spanning roots tags them all under the first resolved root —
        // acceptable: the reap closes the union on teardown.
        let doc_owner: Option<String> = if scoped {
            scoped_files
                .first()
                .and_then(|f| crate::lock::resolve_lock_root(f))
                .map(|r| r.to_string_lossy().into_owned())
        } else {
            bare_roots.first().map(|r| r.to_string_lossy().into_owned())
        };

        // The takeover breadcrumb (root-ownership stage 3, deliverable 7): the
        // first serve after a `catenary claim` reads-and-removes the marker and
        // leads its receipt with the claimed line. Bare pays every served
        // kitchen; a scoped run checks the roots it names. One-shot — a later
        // serve on the same root (no new claim) sees nothing.
        let claimed_roots: Vec<PathBuf> = if scoped {
            let mut roots: Vec<PathBuf> = scoped_files
                .iter()
                .filter_map(|f| crate::lock::resolve_lock_root(f))
                .collect();
            roots.sort();
            roots.dedup();
            roots
        } else {
            bare_roots.clone()
        };
        let claimed = claimed_roots
            .iter()
            .any(|r| crate::lock::take_claim_marker(r));

        // The identity-less session tag used for board reflection / ephemeral
        // mounts / milestones (observability only, never gating): `"default"`
        // when the request carries none, matching the convention hook dispatch
        // applies elsewhere.
        let session_id = raw
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();

        // Mint the scope UUID for this run. (The old prepare-hook parent_id is
        // gone with the two-phase handoff — the serve is one scope.)
        let scope_id = uuid::Uuid::new_v4().to_string();

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
        // dirty files their diagnostics. `covered` is the diagnosed file count:
        // it lets the CLI print `[no edited files]` for a genuinely empty set
        // (covered == 0). The bare-rerun contract retired (root-ownership stage
        // 3, deliverable 6): a bare run after full payment finds an empty ledger
        // and honestly answers `[no edited files]` — no debt, no fault.
        let (dirty, output, covered, fault, delivered) = {
            let covered = diag_files.len();
            if diag_files.is_empty() {
                // Nothing due to diagnose — the honest no-debt answer (the new
                // bare-rerun contract). A `claimed` breadcrumb still leads even an
                // empty receipt so a takeover of a paid root is announced.
                let output = if claimed {
                    CLAIMED_RECEIPT_LEAD.to_string()
                } else {
                    String::new()
                };
                (false, output, covered, None::<String>, Vec::new())
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
                // Per-root diagnose admission (misc 197 stage 1, re-keyed to the
                // root in stage 3): admit one round per ROOT at a time. The host
                // harness auto-backgrounds a slow `catenary diagnostics` and
                // retries it, stacking a fresh concurrent round per retry; left
                // unbounded they all fan out to the shared LSP pool at once. A
                // round whose root already has one in flight WAITS for it here,
                // then runs its own — never a second concurrent execution. The
                // seat is keyed by `doc_owner` (the resolved root); a run with no
                // resolvable root takes none. The guard is held across the whole
                // pipeline below and freed on drop (completion, an early return, or
                // a cancelled future) so a wedged round never locks the root out.
                // `followed` ⇒ this run waited on a prior round, so its receipt
                // earns the one-line note.
                let (_round_guard, followed) = match doc_owner.as_deref() {
                    Some(key) => {
                        let (guard, waited) = ctx.diag_rounds.claim(key).await;
                        (Some(guard), waited)
                    }
                    None => (None, false),
                };
                // Race the diagnostics pipeline against client disconnect. If the
                // `catenary diagnostics` process is killed mid-settle (e.g. the host
                // tool-call timeout fires while a server sits in a `$/progress`
                // bracket), the socket closes, the probe reads EOF, and we drop the
                // pipeline future instead of leaving a settle wait pinned on
                // a Busy server (bug 24). Mirrors the grep/glob cancel-on-disconnect
                // path. The dropped batch self-heals: `open_document_on`'s change
                // gate re-syncs an already-open doc (didChange when its content
                // moved, nothing when unchanged — never a duplicate `didOpen`).
                let raced = race_against_disconnect(
                    ctx.primary.diagnostics.process_files_batched(
                        &diag_files,
                        Some(&scope_id),
                        doc_owner.as_deref(),
                    ),
                    &mut buf_reader,
                )
                .await;
                let Some(outcome) = raced else {
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
                // The delivery set (bug 120): the named paths PLUS the files the
                // pipeline actually served. A directory argument expands to its
                // covered files *inside* the pipeline (`plan_scope`), so those
                // files never appear in `diag_files` — without the served set
                // their ledger entries would survive delivery as phantom debt
                // (the sighting: a directory-form sweep answered "10 files
                // clean", then the Stop blocked naming one of them). Directories
                // themselves are dropped: a directory has no ledger leaf; its
                // expansion carries the payment.
                let delivered: Vec<PathBuf> = diag_files
                    .iter()
                    .chain(outcome.served.iter())
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .filter(|p| !p.is_dir())
                    .collect();
                // Lead the receipt with the one-line takeover breadcrumb when
                // this is the first serve after a `catenary claim` (root-ownership
                // stage 3, deliverable 7), then the misc-197 followed note when
                // this round waited on a prior same-root round.
                let mut receipt = outcome.output;
                if followed {
                    receipt = if receipt.is_empty() {
                        DIAG_FOLLOWED_NOTE.to_string()
                    } else {
                        format!("{DIAG_FOLLOWED_NOTE}\n{receipt}")
                    };
                }
                if claimed {
                    receipt = if receipt.is_empty() {
                        CLAIMED_RECEIPT_LEAD.to_string()
                    } else {
                        format!("{CLAIMED_RECEIPT_LEAD}\n{receipt}")
                    };
                }
                (outcome.dirty, receipt, covered, None::<String>, delivered)
            }
        };

        emit_hook_event(
            tracing::Level::INFO,
            "cli",
            &method,
            Some(&scope_id),
            fault.as_deref().unwrap_or(&output),
            "outgoing hook response",
        );

        // Structured response mirroring the grep/glob JSON envelope. `output` is
        // the rendered per-file receipt the CLI prints verbatim. `status` is a
        // clean/dirty label the CLI no longer maps to an exit code (ws37 ticket
        // 01) — it is retained for telemetry. `covered` (the diagnosed file
        // count) lets the CLI print `[no edited files]` for a genuinely empty set
        // (covered == 0) — the honest no-debt answer of the new bare-rerun
        // contract (root-ownership stage 3). The `fault` arm is retired: a bare
        // run against a paid/empty ledger is not a fault, it is `[no edited
        // files]`, so this always renders a receipt-shaped success.
        let envelope = fault.as_ref().map_or_else(
            || {
                serde_json::json!({
                    "status": if dirty { "dirty" } else { "clean" },
                    "output": output,
                    "covered": covered,
                })
            },
            |message| {
                serde_json::json!({
                    "status": "error",
                    "error": message,
                    "covered": covered,
                })
            },
        );
        let mut payload = serde_json::to_vec(&envelope)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;

        // The response bytes reached the client — pay the debt now (root-ownership
        // stage 3). Delivery deletes: unlink each served file's touch entry from
        // the durable root lock's on-disk ledger — the SOLE payment mechanism now
        // that the ledger is the source of truth (the in-memory identity-keyed
        // batch and its `delivered` flags retired with the handoff). Empty `dir/`
        // = paid = the daemon's idle countdown starts; payment is parole, not
        // release (the lock dir survives, the paid-idle reaper removes it after
        // the window). `unlink_delivered_locks` groups by each file's resolved
        // lock root, so a scoped set spanning kitchens pays each. A failed
        // `write_all` above returned early via `?`, leaving every touch file in
        // place and the gate armed — the killed-client shape recovers by
        // re-running (the next bare re-serves the still-due set, fresh).
        //
        // The delivered set: the named files (or the whole due set the bare run
        // diagnosed) UNION the files the pipeline actually served — a directory
        // argument's expansion pays like a directly-named file (bug 120).
        unlink_delivered_locks(&delivered);
        debug!(
            source = Source::DaemonDispatch.as_str(),
            session_id = %session_id,
            scoped,
            delivered = delivered.len(),
            "diagnostics: served files unlinked from the ledger (root-ownership stage 3)",
        );

        writer.shutdown().await?;
        return Ok(());
    }

    // ── Root claim (root-ownership stage 2) ──────────────────────
    //
    // `pre-tool/claim` is sent by the PreToolUse hook for `catenary claim
    // <root>`: the hook supplies the claimant's identity (the one seam identity
    // appears), the daemon runs the mechanical guard + records the takeover, and
    // stages the rendered answer for the identity-less CLI to drain via
    // `tool/claim`. The rename itself is a filesystem fact — the hook does it
    // locally if the daemon is down (degrade-open), so the guard here is a
    // best-effort safety, not the sole authority.
    if method == "pre-tool/claim" {
        let scope_id = uuid::Uuid::new_v4().to_string();
        let response = handle_claim_stage(&ctx, &raw).await;
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

    if method == "tool/claim" {
        // The identity-less CLI drains the staged claim answer. An absent slot
        // (the hook could not stage — daemon was down at hook time, or the guard
        // refused) yields a `not_staged` status the CLI maps to its own
        // degrade-open path (read the lock state directly).
        let response = match ctx.handoff.consume(HandoffKey::Claim).map(|h| h.payload) {
            Some(HandoffPayload::Claim { answer }) => {
                serde_json::json!({"status": "ok", "answer": answer})
            }
            None => serde_json::json!({"status": "not_staged"}),
        };
        let mut payload = serde_json::to_vec(&response)?;
        payload.push(b'\n');
        writer.write_all(&payload).await?;
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
                // Persistence leg (misc 175): record the pin in the user config's
                // `[roots] pinned` list so it survives a daemon restart. The
                // runtime pin above already took effect; a config-write failure is
                // non-fatal and warned, never bounced back to the command. The
                // config path is injected (bug 109) so an in-process test writes a
                // tempdir, never the operator's real config.
                persist_pin(&ctx.user_config_path, &canonical);
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
                }
                // Persistence leg (misc 175): drop the entry from the user
                // config's `[roots] pinned` list. Runs even when the tracker had
                // no live `hook` contributor — a pin whose path was missing at
                // boot is kept in config but never re-added to the tracker
                // (keep-with-doctor-finding), so `unpin` must be able to remove a
                // config-only entry. `unpin` succeeds when EITHER the live pin OR
                // the config entry was removed; only when NEITHER existed is it
                // the benign idempotent `not_found`. The config path is injected
                // (bug 109) so an in-process test writes a tempdir, not the real
                // config.
                let config_removed = persist_unpin(&ctx.user_config_path, &canonical);
                if removed || config_removed {
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

    // ── Read-action recorder (misc 201, "record ALL reads") ────────────
    //
    // Every read-class host tool that reaches the PreToolUse leg — the Read tool
    // auto-allowed in a working directory, host `Grep`/`Glob` when they aren't
    // denied — records ONE action event to the firehose here, riding the existing
    // `pre-tool/editing-state` IPC. Placed daemon-side because the hook CLI's
    // tracing subscriber carries only the desktop-notification sink (the JSONL
    // firehose subscribes to the DAEMON's tracing), so a hook-side `info!` reaches
    // no archive. Recording only — it never gates the dispatch — and it fires for
    // ALL read-class pre-tool dispatches, subagents included (they route through
    // this same method). `info!` by ruling: firehose only, never a TUI finding or
    // an interrupt (a read is the highest-frequency action). Content is never
    // recorded — only the action's fields (tool, path, session, agent, cwd).
    if method == "pre-tool/editing-state" {
        record_read_action(&raw, &session_id);
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
    // clean turn: not already blocking on the editing gate. Scoped to the
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
        && let Some(nag) = lingering_worktree_nag(&ctx, &session_id, &raw)
    {
        result.result = Some(crate::hook::HookResult::Block(nag));
    }

    // ── Merged-linger advisory at the main agent's Stop (wf-04) ────────
    //
    // The second linger oracle: `git cherry` patch-equivalence. A squash
    // landing creates no ancestry, so a landed worktree lingers until someone
    // runs `catenary worktree rm`; this teaches the forgotten rm. Advisory,
    // never a gate — it rides the `merged_nudge` side-channel (a Claude
    // `systemMessage`) and the require-release outcome is untouched, blocked
    // or allowed. Gated to the MAIN agent (top-level Stop, no agent id): a
    // subagent's stop never draws it. Placed AFTER the lingering nag above so
    // a worktree qualifying under both oracles is marked by that one first and
    // draws exactly one line (the shared once-per-worktree `nagged` ledger is
    // the dedupe).
    let merged_nudge = if method == "post-agent/require-release" && is_main_agent_stop(&raw) {
        merged_worktree_nudge(&ctx, &session_id)
    } else {
        None
    };

    // ── Held-open batch teardown: root retirement, not identity (stage 3) ──
    //
    // The identity-keyed Stop/SubagentStop document close retired with the
    // handoff demolition: documents a diagnose round opens are now tagged with
    // their ROOT (root-ownership stage 3), so they are closed at root retirement
    // (worktree removal — see `retire_root`) and by daemon death (bug 79,
    // unchanged), never by a `(session, agent)` correlation below the hook. The
    // one-cook-per-kitchen durable lock (stage 2) means a root's held-open
    // documents belong to exactly one editor, so root-lifetime teardown closes
    // exactly the right set.

    // ── The "kept" signal: arm the worktree countdown at subagent stop (root-ownership 04) ──
    //
    // A Claude Code SubagentStop reaches the daemon as this same
    // `post-agent/require-release`. Instead of tearing the mount down immediately
    // (the old RAM-buildup fix), arm the KEPT COUNTDOWN on the subagent's worktree
    // mount: the servers stay warm for a `land`/`rm`, and the countdown (reset by
    // any hook resolving into the worktree — the one hook seam) bounds how long an
    // idle kept mount lingers. Expiry retires the MOUNT only (servers, root, lock —
    // `retire_root`), never the dirty directory. A CLEAN worktree is disposed in
    // the background below; its dir removal fires the vanish-watch, which retires
    // the mount at once — the countdown never matters for it.
    //
    // Outcome-gated (maintainer ruling): arm ONLY when the require-release outcome
    // ALLOWS the stop. A `Block` means the agent is NOT stopping — it is about to
    // run `catenary diagnostics` in that very worktree — so the mount stays a LIVE
    // (uncounted) mount until the debt is paid and the `stop_hook_active` retry
    // allows the stop.
    //
    // Resolution is by cwd — pure path algebra, no identity
    // (`resolve_stop_reap_target`; root-ownership 04, AUDIT #10/#11). A pure side
    // effect (decision 029): invisible to the require-release response.
    if method == "post-agent/require-release"
        && let Some(hp) = raw.get("host_payload")
        && hp.get("hook_event_name").and_then(|v| v.as_str()) == Some("SubagentStop")
        && !matches!(&result.result, Some(crate::hook::HookResult::Block(_)))
    {
        let agent_id = raw.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
        // Drop the subagent from the board — it has stopped (the stop is allowed;
        // a blocked stop, gated out above, means it is still running).
        ctx.subagents.stop(&session_id, agent_id);
        let cwd = hp.get("cwd").and_then(|v| v.as_str());
        if let Some((_contributor, worktree)) = resolve_stop_reap_target(&session_id, cwd) {
            // Arm the kept countdown on the mount enclosing the stop's cwd (the
            // "kept" signal). A LIVE mount enters the countdown; the reaper retires
            // it after the idle window unless a hook refreshes it first. Resolved
            // by path — the stopping cwd's enclosing worktree — so no identity is
            // needed. A no-op when the worktree was never a mounted root of ours.
            let armed = ctx.worktree_mounts.arm_countdown(&worktree, Instant::now());
            debug!(
                source = Source::DaemonDispatch.as_str(),
                session_id = %session_id,
                worktree = %worktree.display(),
                armed = armed,
                "subagent stop: armed the worktree kept countdown",
            );

            // misc 151 trigger 1: dispose the subagent's worktree in the
            // background (spawn_blocking for the git subprocesses) so the
            // stop-gate response latency is untouched (decision 029). Clean →
            // auto-disposed (dir removed → the vanish-watch retires the mount);
            // dirty → KEPT untouched, discoverable via the session-start linger
            // nag and `worktree ls`, its mount governed by the countdown just
            // armed. Never auto-cleaned (the maintainer-verbatim absolute).
            //
            // Bug 103 ownership gate: pass the stopping agent's id so the dispose
            // fires only when it IS the worktree's owner (its dirname). The cwd
            // resolution may enclose a live sibling's tree; the gate stops that
            // from disposing a tree the stopping agent does not own.
            let registry = ctx.worktree_registry.clone();
            let sid = session_id.clone();
            let aid = agent_id.to_string();
            let wt = worktree;
            tokio::task::spawn_blocking(move || {
                dispose_worktree_in_background(&registry, &sid, Some(&aid), &wt, false);
            });
        }
    }

    // Edit-mount preheat (root-ownership stage 6, deliverable 3 — the bug-108
    // absorption completes here): the FIRST edit of a file fires the ensure path
    // so a cold root's server starts BEFORE the first `catenary diagnostics`. A
    // no-op in the common case — grep/glob already warmed the server via their
    // own `ensure_ephemeral_mounts` — but an agent that opens with an edit (no
    // prior read) no longer pays the spawn latency inside the diagnose.
    //
    // This rides the EXISTING `pre-tool/editing-state` hook IPC and never gates
    // the edit decision (stage 2's binding constraint): the hook made the
    // one-cook allow/deny call hook-side, filesystem-only, BEFORE any daemon
    // contact, and the edit proceeds identically when the daemon is down (the
    // hook silently no-ops on an unreachable socket). Reaching the daemon here is
    // advisory preheat only. `ensure_ephemeral_mounts` is idempotent per path —
    // an already-mounted root only has its idle clock / kept countdown refreshed,
    // which subsumes the belt-and-suspenders refresh the old code did — so a
    // cold root mounts (server spawns) and a warm one is merely touched.
    if let Some(file_path) = raw.get("file_path").and_then(|v| v.as_str()) {
        let touched =
            resolve_touched_paths(&[PathBuf::from(file_path)], hook_cwd(&raw).map(Path::new));
        ensure_ephemeral_mounts(&ctx, &touched, Instant::now(), &session_id).await;
    }

    // ── SessionStart project-config setup nudge (misc 202) ──────────────
    //
    // When a served root routes to a language server with a project-config-file
    // convention (rust-analyzer → `rust-analyzer.toml`) and that file is absent,
    // surface a one-line pointer so the agent knows its editor/receipt lint+feature
    // surface may not match its build. Once per root per daemon instance (the
    // `ProjectConfigNudges` ledger): a repeat `SessionStart` on the same root is
    // silent. Computed here because the roots ride the SessionStart host payload
    // (`workspacePaths`/`cwd`) — they are not yet in the RootTracker at this seam.
    let session_start_nudge = if method == "session-start/clear-editing" {
        session_start_project_config_nudge(&ctx, &raw)
    } else {
        None
    };

    // ── SessionStart auto-install detection (lsm 05) ─────────────────────
    //
    // Opt-in (`[servers] auto_install`, user-config only): detect blessed
    // servers the session's roots want but cannot spawn, and kick each as a
    // daemon-side background task. The kick is a spawn, never an await —
    // session-start latency is flat whether or not an install runs. The
    // returned announcement rides the response; the CLI surfaces it on the
    // user-visible `systemMessage` channel.
    let auto_install_announcement = if method == "session-start/clear-editing" {
        session_start_auto_install(&ctx, &raw)
    } else {
        None
    };

    let envelope = HookResponseEnvelope {
        result: result.result,
    };

    let response = if session_start_nudge.is_some()
        || merged_nudge.is_some()
        || auto_install_announcement.is_some()
    {
        // A nudge rides alongside any result on its own wire field —
        // `session_start_nudge` (misc 202; the CLI folds it into the
        // SessionStart context), `merged_nudge` (wf-04; the CLI surfaces it
        // as a Stop-time `systemMessage`, never a gate), or
        // `auto_install_announcement` (lsm 05; a SessionStart-time
        // `systemMessage`). A hook that never nudges keeps the plain envelope.
        let mut obj = serde_json::to_value(&envelope)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        if let Some(nudge) = session_start_nudge {
            obj.insert(
                "session_start_nudge".to_string(),
                serde_json::Value::String(nudge),
            );
        }
        if let Some(nudge) = merged_nudge {
            obj.insert("merged_nudge".to_string(), serde_json::Value::String(nudge));
        }
        if let Some(announcement) = auto_install_announcement {
            obj.insert(
                "auto_install_announcement".to_string(),
                serde_json::Value::String(announcement),
            );
        }
        serde_json::to_string(&serde_json::Value::Object(obj))?
    } else if envelope.result.is_some() {
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

/// Maximum number of attempts [`ensure_daemon_running`] (`catenary start`)
/// makes to reach a spawned daemon's socket before reporting failure.
///
/// This budget belongs to the explicit CLI verb only. The bridge's own connect
/// paths retired their give-up budgets (workstream "pulse"): the bridge never
/// kills itself, so its loops retry indefinitely via
/// [`connect_with_tenacity`].
const MAX_CONNECT_ATTEMPTS: u32 = 10;

/// Delay between connection retry attempts in [`ensure_daemon_running`].
const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Backoff floor for the bridge's indefinite connect loop (pulse 02).
const CONNECT_BACKOFF_FLOOR: std::time::Duration = std::time::Duration::from_millis(100);

/// Backoff cap for the bridge's indefinite connect loop (pulse 02).
const CONNECT_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(5);

/// How much accumulated waiting elapses between "still waiting" progress logs
/// in the bridge's indefinite connect loop (pulse 02).
const WAIT_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How much accumulated waiting must pass after a daemon spawn before the
/// bridge's connect loop concludes the spawn failed and respawns (pulse 02).
///
/// Long enough that a daemon binding slowly under heavy load is never doubled
/// up on; short enough that a spawn that crashed before binding is retried
/// promptly — the loop is indefinite, so this paces respawns rather than
/// bounding them.
const SPAWN_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs the bridge proxy: connect-or-start the daemon, then proxy
/// stdin/stdout to/from the daemon socket.
///
/// Entirely synchronous — no tokio runtime involvement in the data
/// path. This avoids any interaction between the tokio runtime's
/// internal epoll/signal state and the blocking I/O threads.
///
/// The startup connect retired its give-up budget (pulse 02): the bridge waits
/// indefinitely for a daemon (the host owns the stdio link, so the bridge
/// never kills itself), exiting cleanly only when the intent marker says
/// `quit`.
///
/// # Errors
///
/// Returns an error if the bridge recursed into itself, the version handshake
/// fails, or the proxy hits a genuine non-retry failure.
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
    // Startup cannot observe stdin without consuming handshake bytes, so the
    // stdin probe always answers "open" here — the loop runs until a daemon
    // answers or the intent marker says quit. A host that hangs up mid-wait
    // kills the process (or the handshake sees EOF right after connect).
    let stream = match connect_or_start_daemon(|| true) {
        TenaciousOutcome::Connected(stream) => stream,
        TenaciousOutcome::StdinClosed | TenaciousOutcome::QuitRequested => return Ok(()),
    };
    proxy_stdio(stream)
}

/// The outcome of the bridge's tenacious connect loop (pulse 02).
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum TenaciousOutcome<T> {
    /// A daemon connection was established.
    Connected(T),
    /// The host's stdin closed while waiting — the only unconditional
    /// self-exit (the host hung up; the stdio link is already gone).
    StdinClosed,
    /// The intent marker said `quit` — the one marker-sanctioned self-exit.
    QuitRequested,
}

/// Runs one indefinite connect-or-spawn wait (pulse 02).
///
/// The bridge never kills itself: self-exit destroys the host↔bridge stdio
/// link from the wrong side, and recovery then depends on host mercy. So this
/// loop retries with capped exponential backoff ([`CONNECT_BACKOFF_FLOOR`]
/// doubling to [`CONNECT_BACKOFF_CAP`]) for as long as the host's stdin is
/// open, consulting the daemon intent marker each tick — at socket-loss and at
/// spawn-time alike. A marker appearing mid-wait takes effect on the next
/// tick.
///
/// Decision table, consulted every tick:
///
/// - stdin closed → [`TenaciousOutcome::StdinClosed`] (the only unconditional
///   self-exit).
/// - marker `quit` → [`TenaciousOutcome::QuitRequested`] (the one
///   marker-sanctioned self-exit), checked before connecting so a quit is
///   obeyed promptly.
/// - marker `stop` → connect-only: keep trying the socket, never spawn. A
///   later `catenary start` clears the marker, so the next tick may spawn
///   again.
/// - marker absent → connect-or-spawn (the ordinary crash path): respawns are
///   paced by [`SPAWN_RETRY_INTERVAL`] of accumulated waiting, so a daemon
///   binding slowly under load is never doubled up on while a spawn that died
///   before binding is still retried.
///
/// Fully closure-parameterized so the retry policy is unit-testable without
/// sockets, daemons, or wall-clock sleeps.
#[cfg(unix)]
fn connect_with_tenacity<T>(
    mut try_connect: impl FnMut() -> Option<T>,
    mut spawn: impl FnMut() -> Result<()>,
    mut stdin_open: impl FnMut() -> bool,
    mut read_intent: impl FnMut() -> Option<crate::daemon_intent::Intent>,
    mut sleep: impl FnMut(std::time::Duration),
) -> TenaciousOutcome<T> {
    use crate::daemon_intent::Intent;

    let mut backoff = CONNECT_BACKOFF_FLOOR;
    let mut waited_total = Duration::ZERO;
    let mut since_progress = Duration::ZERO;
    // Accumulated waiting since the last spawn attempt; `None` while no spawn
    // is in flight (a spawn is then allowed immediately).
    let mut since_spawn: Option<Duration> = None;
    let mut waiting = false;

    loop {
        if !stdin_open() {
            info!(
                source = Source::DaemonLifecycle.as_str(),
                "stdin closed while waiting for the daemon — ending bridge session",
            );
            return TenaciousOutcome::StdinClosed;
        }
        let intent = read_intent();
        let mode = intent.map_or("absent", Intent::as_str);
        if intent == Some(Intent::Quit) {
            info!(
                source = Source::DaemonLifecycle.as_str(),
                "daemon.intent says quit — bridge obeying the one sanctioned self-exit",
            );
            return TenaciousOutcome::QuitRequested;
        }
        if let Some(stream) = try_connect() {
            if waiting {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    waited_secs = waited_total.as_secs(),
                    "daemon link healed — connected after waiting",
                );
            } else {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    "connected to daemon",
                );
            }
            return TenaciousOutcome::Connected(stream);
        }

        if !waiting {
            waiting = true;
            info!(
                source = Source::DaemonLifecycle.as_str(),
                mode, "daemon unreachable — bridge entering wait mode (indefinite retry)",
            );
        }

        if intent.is_none() {
            // Connect-or-spawn: today's crash path. Spawn when none is in
            // flight or the last one has outlived its grace window.
            let spawn_due = since_spawn.is_none_or(|since| since >= SPAWN_RETRY_INTERVAL);
            if spawn_due {
                match spawn() {
                    Ok(()) => since_spawn = Some(Duration::ZERO),
                    Err(e) => {
                        // A failed spawn launched nothing — leave the pacing
                        // unarmed so the next tick retries it.
                        debug!(
                            source = Source::DaemonLifecycle.as_str(),
                            error = %e,
                            "daemon spawn attempt failed — will retry",
                        );
                    }
                }
            }
        } else {
            // `stop`: connect-only — never spawn while the daemon is
            // deliberately down. Reset the pacing so a cleared marker may
            // spawn on its very next tick.
            since_spawn = None;
        }

        debug!(
            source = Source::DaemonLifecycle.as_str(),
            mode,
            backoff_ms = backoff.as_millis(),
            "daemon connect tick failed — backing off",
        );
        sleep(backoff);
        waited_total = waited_total.saturating_add(backoff);
        since_progress = since_progress.saturating_add(backoff);
        if let Some(since) = &mut since_spawn {
            *since = since.saturating_add(backoff);
        }
        if since_progress >= WAIT_PROGRESS_INTERVAL {
            since_progress = Duration::ZERO;
            info!(
                source = Source::DaemonLifecycle.as_str(),
                mode,
                waited_secs = waited_total.as_secs(),
                "still waiting for the daemon",
            );
        }
        backoff = (backoff * 2).min(CONNECT_BACKOFF_CAP);
    }
}

/// Connects to a running daemon or starts one, waiting indefinitely (pulse 02).
///
/// Each tick tries the MCP socket; when the intent marker allows a spawn, it
/// clears stale socket files and spawns `catenary daemon` — the same
/// single-instance start path `catenary start` uses. Never gives up while
/// `stdin_open` answers true; see [`connect_with_tenacity`] for the decision
/// table.
#[cfg(unix)]
fn connect_or_start_daemon(
    stdin_open: impl FnMut() -> bool,
) -> TenaciousOutcome<std::os::unix::net::UnixStream> {
    let mcp_path = mcp_socket_path();
    connect_with_tenacity(
        || std::os::unix::net::UnixStream::connect(&mcp_path).ok(),
        || {
            // Clear stale socket files (a crashed daemon may leave them), then
            // spawn through the shared path.
            if mcp_path.exists() {
                let _ = std::fs::remove_file(&mcp_path);
            }
            let ipc_path = socket_path();
            if ipc_path.exists() {
                let _ = std::fs::remove_file(&ipc_path);
            }
            spawn_daemon()
        },
        stdin_open,
        crate::daemon_intent::read,
        std::thread::sleep,
    )
}

/// The outcome of an idempotent `catenary start` (bug 80, leg 2).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStartOutcome {
    /// A daemon was already up; `start` connected and left it running.
    AlreadyRunning,
    /// No daemon was up; `start` spawned one and confirmed it bound its sockets.
    Started,
}

/// Starts the Catenary daemon explicitly and idempotently (bug 80, leg 2).
///
/// The `stop` counterpart: brings the daemon up through the **same**
/// single-instance start path the bridge init uses ([`spawn_daemon`] + the same
/// stale-socket cleanup and attempt-structured connect retry), so a manual
/// `catenary stop` or a killed daemon has a one-command remedy.
///
/// Idempotent: probes the IPC socket first — a successful connect means a daemon
/// is already up, so it reports [`DaemonStartOutcome::AlreadyRunning`] and
/// spawns nothing. The probe uses the **IPC** socket, never the MCP socket:
/// an MCP connect-and-drop would register then release the only connection and
/// trip the daemon's last-disconnect shutdown, whereas an IPC probe never holds
/// the daemon alive.
///
/// When no daemon answers, it clears any stale socket files, spawns one, and
/// waits — attempt-bounded, not wall-clock — for the IPC socket to accept a
/// connection, returning [`DaemonStartOutcome::Started`].
///
/// # Errors
///
/// Returns an error if the daemon cannot be spawned or does not bind its sockets
/// within [`MAX_CONNECT_ATTEMPTS`].
#[cfg(unix)]
pub fn ensure_daemon_running() -> Result<DaemonStartOutcome> {
    let ipc_path = socket_path();

    // Idempotent fast path: a live IPC socket means a daemon is already up.
    if std::os::unix::net::UnixStream::connect(&ipc_path).is_ok() {
        return Ok(DaemonStartOutcome::AlreadyRunning);
    }

    // No daemon answered. Clear stale socket files (a clean daemon exit removes
    // them, but a crash may leave them), then spawn through the shared path.
    let mcp_path = mcp_socket_path();
    if mcp_path.exists() {
        let _ = std::fs::remove_file(&mcp_path);
    }
    if ipc_path.exists() {
        let _ = std::fs::remove_file(&ipc_path);
    }
    spawn_daemon()?;

    // Wait — attempt-structured, not test-observable timing — for the daemon to
    // bind its IPC socket and accept a connection.
    for attempt in 0..MAX_CONNECT_ATTEMPTS {
        if std::os::unix::net::UnixStream::connect(&ipc_path).is_ok() {
            info!(
                source = Source::DaemonLifecycle.as_str(),
                attempt, "daemon started via `catenary start`",
            );
            return Ok(DaemonStartOutcome::Started);
        }
        std::thread::sleep(CONNECT_RETRY_DELAY);
    }

    anyhow::bail!(
        "daemon spawned but did not bind its socket \
         after {MAX_CONNECT_ATTEMPTS} attempts ({})",
        ipc_path.display(),
    )
}

/// Heals the Linux rename-swap artifact in the bridge's own path (misc 182).
///
/// After a binary swap under a running bridge (`cargo install --path .` /
/// `catenary update` rename over the file), `/proc/self/exe` resolves to
/// `<path> (deleted)` — the dead inode's name, not the living file. The
/// rename left the NEW binary at the original path, and exec'ing the path is
/// exactly what the respawn wants — so the marker is trimmed when (and only
/// when) the trimmed path exists again. A binary genuinely named
/// `… (deleted)` passes through untouched: its trimmed sibling doesn't
/// exist. macOS is unaffected (`current_exe` there answers the original
/// path, which the rename refreshed in place). Kept on `current_exe`, not a
/// PATH lookup — test hermeticity depends on it (`CARGO_BIN_EXE` binaries
/// are not on PATH, and `isolate_env` clears PATH).
#[cfg(unix)]
fn heal_swapped_exe(exe: PathBuf) -> PathBuf {
    let Some(trimmed) = exe
        .to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map(PathBuf::from)
    else {
        return exe;
    };
    if trimmed.is_file() {
        info!(
            source = Source::DaemonLifecycle.as_str(),
            healed = %trimmed.display(),
            "respawn path carried the ` (deleted)` rename-swap marker; exec'ing the living path",
        );
        trimmed
    } else {
        exe
    }
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

    let exe = heal_swapped_exe(std::env::current_exe().context("resolve current executable path")?);

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
    // Phase 1: Version handshake (blocking, sequential). Intercepts the first
    // MCP exchange (initialize) to verify the daemon version, and captures the
    // initialize request line so the reconnect path can replay it (bug 80).
    let init_line = {
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        version_handshake(&mut stdin, &stream, &mut stdout)?
    };

    // Phase 2: Reconnect-aware byte proxy (bug 80, leg 1). A mid-session daemon
    // loss is a transparent blip: the reader thread reconnects (respawning the
    // daemon through the same single-instance path when absent), replays the
    // captured initialize against the fresh daemon, and resumes — the host↔bridge
    // stdio link never breaks, so the MCP server stays "connected" for real.
    proxy_with_reconnect(stream, &init_line)
}

/// The swappable current daemon write-half plus a generation counter (bug 80,
/// leg 1).
///
/// The reader thread owns reconnection: on daemon loss it installs a fresh
/// write-half here and bumps `generation`. The writer (stdin) thread writes to
/// the current half; on failure it waits on the condvar for a newer generation,
/// then retries the pending line against the reconnected socket. `done` is set
/// when either direction ends terminally (stdin EOF, stdout gone, or a
/// marker-sanctioned quit) so the other side stops.
#[cfg(unix)]
struct SocketSlot {
    /// The current daemon write-half, or `None` once the proxy is done.
    writer: Option<std::os::unix::net::UnixStream>,
    /// Bumped on every reconnect so the writer can detect a fresh socket.
    generation: u64,
    /// Terminal flag: no more reconnects, both directions should exit.
    done: bool,
    /// `false` while a daemon loss is known but not yet healed (the reader set
    /// it on detecting loss and clears it after reinstalling a fresh writer).
    /// The writer refuses to write while unhealthy — a write to a just-dead
    /// socket can succeed *silently* (the bytes land in a kernel buffer the RST
    /// discards), so waiting for the heal is the only way to guarantee a host
    /// request written **after** the loss is observed reaches the fresh daemon,
    /// not the void.
    healthy: bool,
}

/// Runs the reconnect-aware MCP byte proxy (bug 80, leg 1).
///
/// Two blocking threads share a [`SocketSlot`] behind a `Mutex`/`Condvar`:
///
/// - **reader** (this fn's own loop): reads daemon→stdout. On EOF/error it
///   reconnects via [`reconnect_daemon`] — respawning the daemon through the
///   same single-instance path the init used, then replaying `init_line` —
///   installs the fresh write-half into the slot, bumps the generation, notifies
///   the writer, and resumes reading from the new socket. Reconnection is
///   indefinite (pulse 02): capped exponential backoff for as long as stdin is
///   open, consulting the daemon intent marker each tick — the bridge never
///   kills itself.
/// - **writer**: reads stdin→daemon line by line. On a write failure (the daemon
///   just died) it waits for a newer generation, then rewrites the same line to
///   the reconnected socket, so no host request is dropped by the swap.
///
/// Returns `Ok(())` when stdin closes (host ended the session), the stdout
/// pipe breaks (host killed the process), or the intent marker says `quit`
/// (the one marker-sanctioned self-exit); `Err` only on a genuine non-retry
/// failure (e.g. cloning the reconnected socket).
#[cfg(unix)]
#[allow(
    clippy::too_many_lines,
    reason = "the reconnect proxy is one cohesive state machine — the writer thread, the reader/reconnect loop, and their shared-slot coordination read most clearly together"
)]
fn proxy_with_reconnect(stream: std::os::unix::net::UnixStream, init_line: &str) -> Result<()> {
    use std::io::{Read, Write};
    use std::sync::{Arc, Condvar, Mutex};

    let write_half = stream.try_clone().context("clone daemon socket")?;
    let slot = Arc::new((
        Mutex::new(SocketSlot {
            writer: Some(write_half),
            generation: 0,
            done: false,
            healthy: true,
        }),
        Condvar::new(),
    ));

    // stdin → daemon: writes each line to the current socket; on failure, waits
    // for a reconnect (newer generation) and retries the same line.
    let writer_slot = Arc::clone(&slot);
    let stdin_thread = std::thread::spawn(move || {
        let (lock, cvar) = &*writer_slot;
        let mut stdin = std::io::stdin().lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            // Read one newline-delimited JSON-RPC message.
            let mut byte = [0u8; 1];
            let read = loop {
                match stdin.read(&mut byte) {
                    Ok(0) => break Ok(false), // stdin EOF
                    Ok(_) => {
                        line.push(byte[0]);
                        if byte[0] == b'\n' {
                            break Ok(true);
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            match read {
                Ok(false) | Err(_) => {
                    // stdin closed — signal done and stop.
                    if let Ok(mut slot) = lock.lock() {
                        slot.done = true;
                        if let Some(w) = slot.writer.take() {
                            let _ = w.shutdown(std::net::Shutdown::Write);
                        }
                    }
                    cvar.notify_all();
                    return;
                }
                Ok(true) => {}
            }
            // Write this line to the current socket, retrying across reconnects.
            loop {
                // Acquire the current write-half. Wait while the slot is
                // momentarily empty OR the daemon is known-lost-but-not-yet-healed
                // — writing during that window would silently drop the request
                // into a dead socket. Single lock guard — no re-entrancy.
                let (mut sock, generation) = {
                    let mut slot = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    while !slot.done && (!slot.healthy || slot.writer.is_none()) {
                        slot = cvar
                            .wait(slot)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if slot.done {
                        return;
                    }
                    match slot.writer.as_ref().and_then(|w| w.try_clone().ok()) {
                        Some(w) => (w, slot.generation),
                        None => continue,
                    }
                };
                if sock.write_all(&line).and_then(|()| sock.flush()).is_ok() {
                    break;
                }
                // Write failed — the daemon died. Wait for a generation newer
                // than the one we just failed on, then retry the same line.
                let failed_gen = generation;
                let mut slot = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !slot.done && slot.generation <= failed_gen {
                    slot = cvar
                        .wait(slot)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if slot.done {
                    return;
                }
            }
        }
    });

    // daemon → stdout: this thread. On daemon loss (EOF *or* a read error like
    // ECONNRESET from a SIGKILLed daemon), reconnect and resume.
    let mut stdout = std::io::stdout().lock();
    let mut buf = vec![0u8; 8192];
    let mut reader = stream;
    let result: Result<()> = loop {
        // A daemon read that returns 0 bytes (clean EOF) *or* fails (ECONNRESET,
        // broken pipe, …) means the daemon is gone — both route to reconnect. A
        // `kill -9` yields ECONNRESET, not EOF, so treating only EOF as loss (the
        // pre-bug-80 behavior) would strand the very case bug 80 filed.
        match reader.read(&mut buf) {
            Ok(n) if n > 0 => {
                if let Err(e) = stdout.write_all(&buf[..n]) {
                    break Err(anyhow::Error::from(e).context("write to stdout"));
                }
                if let Err(e) = stdout.flush() {
                    break Err(anyhow::Error::from(e).context("flush stdout"));
                }
                continue;
            }
            // `Ok(0)` (clean EOF) or `Err(_)` (ECONNRESET, EPIPE, …): daemon gone.
            _ => {}
        }

        // Daemon gone. Mark unhealthy at once so the writer stops writing into
        // the dead socket (a silent-loss window) until the reconnect heals it. If
        // stdin already ended, this is a clean exit.
        {
            let (lock, cvar) = &*slot;
            if let Ok(mut s) = lock.lock() {
                if s.done {
                    break Ok(());
                }
                s.healthy = false;
            }
            cvar.notify_all();
        }
        // Reconnect: respawn the daemon (if absent) and replay init, waiting
        // indefinitely — for as long as the host's stdin stays open (pulse 02).
        // The writer thread sets `done` on stdin EOF, so the wait consults it
        // each tick.
        let outcome = reconnect_daemon(init_line, || {
            let (lock, _cvar) = &*slot;
            let s = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            !s.done
        });
        match outcome {
            ReconnectOutcome::Reconnected(fresh) => {
                let new_write = match fresh.try_clone().context("clone reconnected socket") {
                    Ok(w) => w,
                    Err(e) => break Err(e),
                };
                let (lock, cvar) = &*slot;
                if let Ok(mut s) = lock.lock() {
                    if s.done {
                        break Ok(());
                    }
                    s.writer = Some(new_write);
                    s.generation += 1;
                    s.healthy = true;
                }
                cvar.notify_all();
                reader = fresh;
            }
            // Stdin closed mid-wait: the host hung up — a clean exit, exactly
            // as if EOF had landed while connected.
            ReconnectOutcome::StdinClosed => break Ok(()),
            // The one marker-sanctioned self-exit: stop both directions and
            // end the session cleanly.
            ReconnectOutcome::QuitRequested => {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    "daemon.intent quit obeyed — ending bridge session",
                );
                let (lock, cvar) = &*slot;
                if let Ok(mut s) = lock.lock() {
                    s.done = true;
                    s.writer = None;
                }
                cvar.notify_all();
                break Ok(());
            }
        }
    };

    // On stdin-EOF the writer thread already exited; otherwise it observes
    // `done` (or the process exits). Don't block on the join.
    drop(stdin_thread);
    result
}

/// The outcome of a mid-session reconnect wait (pulse 02).
#[cfg(unix)]
enum ReconnectOutcome {
    /// A fresh daemon socket, initialize replayed and swallowed — resume the
    /// byte proxy on it.
    Reconnected(std::os::unix::net::UnixStream),
    /// The host's stdin closed mid-wait — end the session cleanly.
    StdinClosed,
    /// The intent marker said `quit` — end the session cleanly.
    QuitRequested,
}

/// Reconnects to the daemon after a mid-session loss, respawning it if absent
/// and replaying the captured initialize (bug 80, leg 1).
///
/// The round budget retired (pulse 02): the wait is indefinite, so this never
/// "exhausts". Each round uses [`connect_or_start_daemon`] — the exact
/// single-instance start-or-connect path the bridge init took, itself an
/// indefinite capped-backoff wait that consults the daemon intent marker each
/// tick — then replays `init_line` against the fresh daemon and **swallows**
/// its initialize response (the host already received one at session start; a
/// second would corrupt the MCP stream). A daemon that dies mid-handshake (a
/// respawn racing its own predecessor's teardown) just starts another round.
/// The only exits are the fresh socket, stdin EOF (`stdin_open` answering
/// false), and a `quit` marker.
#[cfg(unix)]
fn reconnect_daemon(init_line: &str, mut stdin_open: impl FnMut() -> bool) -> ReconnectOutcome {
    use std::io::Write;

    let mut round: u64 = 0;
    loop {
        let socket = match connect_or_start_daemon(&mut stdin_open) {
            TenaciousOutcome::Connected(s) => s,
            TenaciousOutcome::StdinClosed => return ReconnectOutcome::StdinClosed,
            TenaciousOutcome::QuitRequested => return ReconnectOutcome::QuitRequested,
        };

        // Replay the captured initialize so the fresh daemon rebuilds the MCP
        // session, then re-announce our version (ws41-02) — a reconnect after a
        // binary swap hands the session to a *fresh* daemon that never saw the
        // first hello, so without this the mismatch a swap introduces would go
        // undetected. Swallow the initialize response — the host already saw the
        // first one.
        let replay = (&socket)
            .write_all(init_line.as_bytes())
            .and_then(|()| (&socket).flush())
            .map_err(anyhow::Error::from)
            .inspect(|()| send_bridge_hello(&socket))
            .and_then(|()| read_json_line(&socket));
        match replay {
            Ok(_response) => {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    round, "reconnected to daemon and replayed initialize",
                );
                return ReconnectOutcome::Reconnected(socket);
            }
            Err(e) => {
                // The daemon we just reached died mid-handshake. Pace the next
                // round so a flapping daemon never spins this loop hot.
                debug!(
                    source = Source::DaemonLifecycle.as_str(),
                    round,
                    error = %e,
                    "reconnected daemon died mid-handshake — retrying",
                );
                round += 1;
                std::thread::sleep(CONNECT_BACKOFF_FLOOR);
            }
        }
    }
}

/// Announces the bridge's protocol version to the daemon during the handshake
/// (ws41-02).
///
/// Reads the MCP `initialize` request from `client`, forwards it to `socket`,
/// then sends a [`catenary_mcp::protocol::BRIDGE_HELLO_METHOD`] notification
/// carrying the bridge's compiled [`catenary_mcp::version`]. The daemon compares
/// that against the version IT links and owns the mismatch surfacing — so a
/// running bridge is **never** torn down here for a version disagreement (a
/// bridge survives binary swaps indefinitely; `/mcp` is needed only when the
/// daemon-side surfacing says so). The hello rides alongside `initialize`, not
/// inside it, so the MCP payload the host sees is untouched.
///
/// The daemon's `initialize` response is read and forwarded to `output`
/// unconditionally. Generic over reader/writer for testability — `proxy_stdio`
/// passes stdin/stdout, tests pass in-memory buffers.
///
/// Returns the captured initialize request line so the reconnect path (bug 80,
/// leg 1) can replay it against a fresh daemon after a mid-session daemon loss —
/// re-establishing the MCP session without the host re-driving `initialize`.
#[cfg(unix)]
fn version_handshake<R: std::io::BufRead, W: std::io::Write>(
    client: &mut R,
    socket: &std::os::unix::net::UnixStream,
    output: &mut W,
) -> Result<String> {
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

    // Announce our compiled protocol version to the daemon — direction-blind
    // comparison and all surfacing happen daemon-side, so this is fire-and-
    // forget: a delivery failure never blocks the session, and a mismatch never
    // tears the bridge down.
    send_bridge_hello(socket);

    // Read the initialize response from daemon.
    // Byte-by-byte to avoid consuming data beyond the line boundary,
    // which would be lost to the subsequent concurrent byte proxy.
    let response_line = read_json_line(socket).context("read initialize response from daemon")?;

    // Forward the daemon's initialize response to the client verbatim — the
    // handshake never rewrites or withholds it.
    output
        .write_all(response_line.as_bytes())
        .context("forward initialize response to client")?;
    output.flush()?;

    Ok(init_line)
}

/// Sends the bridge's version hello to the daemon, fire-and-forget (ws41-02).
///
/// Writes a [`catenary_mcp::protocol::BRIDGE_HELLO_METHOD`] notification
/// carrying the bridge's compiled [`catenary_mcp::version`]. Delivery failures
/// are ignored: comparison and all surfacing are daemon-side, so a lost hello
/// only means the daemon does not learn this bridge's version now — it never
/// blocks or tears down the session. Sent on the initial handshake **and** on
/// every reconnect (bug 80), because a reconnect after a binary swap is exactly
/// the case a fresh daemon must be told the bridge's version to detect a
/// mismatch.
#[cfg(unix)]
fn send_bridge_hello(socket: &std::os::unix::net::UnixStream) {
    use std::io::Write;

    let hello = serde_json::json!({
        "jsonrpc": "2.0",
        "method": catenary_mcp::protocol::BRIDGE_HELLO_METHOD,
        "params": { "bridgeVersion": catenary_mcp::version() }
    });
    if let Ok(line) = serde_json::to_string(&hello) {
        let _ = (&*socket).write_all(line.as_bytes());
        let _ = (&*socket).write_all(b"\n");
        let _ = (&*socket).flush();
    }
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
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
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

    /// Polls `cond` until it holds, capped at 5 s. Progress-aware
    /// synchronization for the lifecycle tests — the test advances the moment
    /// the condition holds instead of sleeping a fixed wall-clock interval.
    async fn wait_until(what: &str, cond: impl Fn() -> bool + Send + Sync) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !cond() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("condition not reached within 5s: {what}"));
    }

    // ── SocketCleanupGuard (bug 111) ───────────────────────────────

    /// An armed guard unlinks both socket files on drop — the failed-boot
    /// cleanup that stops a stranded socket from provoking the "unreachable"
    /// storm.
    #[test]
    fn socket_cleanup_guard_armed_unlinks_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mcp = dir.path().join("mcp.sock");
        let ipc = dir.path().join("ipc.sock");
        std::fs::write(&mcp, b"").expect("write mcp");
        std::fs::write(&ipc, b"").expect("write ipc");

        {
            let _guard = SocketCleanupGuard {
                mcp_path: mcp.clone(),
                ipc_path: ipc.clone(),
                armed: true,
            };
        } // drop here

        assert!(
            !mcp.exists(),
            "armed guard must unlink the MCP socket on drop"
        );
        assert!(
            !ipc.exists(),
            "armed guard must unlink the IPC socket on drop"
        );
    }

    /// A disarmed guard leaves the socket files intact — the success path, where
    /// the `SessionManager` has taken over the socket lifetime.
    #[test]
    fn socket_cleanup_guard_disarmed_leaves_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mcp = dir.path().join("mcp.sock");
        let ipc = dir.path().join("ipc.sock");
        std::fs::write(&mcp, b"").expect("write mcp");
        std::fs::write(&ipc, b"").expect("write ipc");

        {
            let mut guard = SocketCleanupGuard {
                mcp_path: mcp.clone(),
                ipc_path: ipc.clone(),
                armed: true,
            };
            guard.disarm();
        } // drop here

        assert!(mcp.exists(), "disarmed guard must leave the MCP socket");
        assert!(ipc.exists(), "disarmed guard must leave the IPC socket");
    }

    /// `from_sockets` disarms the boot-abort guard as it takes ownership: after
    /// the manager drops, the sockets are gone (its OWN drop cleans up), but the
    /// `DaemonSockets` guard must NOT have fired early and left the manager
    /// serving on an unlinked socket. This proves the disarm handoff.
    #[tokio::test]
    async fn from_sockets_disarms_guard_and_manager_owns_cleanup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mcp = mcp_socket_in(dir.path());
        let ipc = ipc_socket_in(dir.path());
        if let Some(parent) = mcp.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }

        let sockets = bind_daemon_sockets_at(&mcp, &ipc).expect("bind");
        assert!(mcp.exists() && ipc.exists(), "bind creates both sockets");

        let manager = SessionManager::from_sockets(sockets, LoggingServer::new());
        // The manager is live: the guard was disarmed, so the sockets still exist.
        assert!(
            mcp.exists() && ipc.exists(),
            "from_sockets must disarm the guard — sockets stay bound while the manager lives",
        );

        drop(manager);
        // The SessionManager's own Drop removes them.
        assert!(
            !mcp.exists() && !ipc.exists(),
            "SessionManager drop unlinks the sockets it took over",
        );
    }

    /// A `DaemonSockets` dropped WITHOUT being consumed (an aborted boot) unlinks
    /// both bound sockets — the stranded-socket fix in isolation.
    #[tokio::test]
    async fn dropped_daemon_sockets_unlink_on_abort() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mcp = mcp_socket_in(dir.path());
        let ipc = ipc_socket_in(dir.path());
        if let Some(parent) = mcp.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }

        {
            let _sockets = bind_daemon_sockets_at(&mcp, &ipc).expect("bind");
            assert!(mcp.exists() && ipc.exists(), "bind creates both sockets");
            // Simulate a post-bind boot abort: drop without from_sockets.
        }

        assert!(
            !mcp.exists() && !ipc.exists(),
            "an unconsumed DaemonSockets must unlink both sockets on drop (bug 111)",
        );
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
        const GRACE: std::time::Duration = std::time::Duration::from_millis(50);

        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());
        let ipc_path = ipc_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()).disconnect_grace_override(GRACE));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Connect one client.
        let stream = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect");
        wait_until("one connection counted", || manager.connection_count() == 1).await;

        // Disconnect — last client gone, nothing reconnects, so the loop
        // exits once the grace window expires (pulse-03: debounced, not
        // immediate).
        let dropped_at = std::time::Instant::now();
        drop(stream);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");
        assert!(
            dropped_at.elapsed() >= GRACE,
            "exit must wait out the grace window, not fire on the disconnect",
        );

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

        let manager = Arc::new(
            bind_in(dir.path()).disconnect_grace_override(std::time::Duration::from_millis(50)),
        );
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Connect two clients.
        let stream1 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 1");
        let stream2 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 2");
        wait_until("two connections counted", || {
            manager.connection_count() == 2
        })
        .await;

        // Disconnect first — daemon should stay alive, and a non-last
        // disconnect never arms the exit window.
        drop(stream1);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "accept_loop should still be running with one client",
        );
        assert!(
            !manager.grace_armed.load(Ordering::Acquire),
            "a disconnect that leaves clients connected must not arm the exit window",
        );

        // Disconnect second — daemon should exit after the grace window.
        drop(stream2);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");

        assert!(result.is_ok(), "accept_loop should return Ok");
    }

    /// Bridge churn survival (pulse-03, acceptance 1): the census drops to
    /// zero, the exit window arms, and a client reconnecting within the
    /// window disarms it — the daemon (and its warm LSP fleet) survives. The
    /// production 60 s grace stays in place so the test never races the
    /// clock: it sequences on the armed flag, not on elapsed time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grace_window_disarmed_by_reconnect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mcp_path = mcp_socket_in(dir.path());

        let manager = Arc::new(bind_in(dir.path()));
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        let stream1 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 1");
        wait_until("one connection counted", || manager.connection_count() == 1).await;

        // Last client drops: the census hits zero and the loop arms the exit
        // window instead of dying.
        drop(stream1);
        wait_until("exit window armed", || {
            manager.grace_armed.load(Ordering::Acquire)
        })
        .await;
        assert!(
            !handle.is_finished(),
            "arming the window must not exit the loop",
        );

        // A client returns within the window: the accept disarms the exit
        // and the loop keeps serving.
        let stream2 = tokio::net::UnixStream::connect(&mcp_path)
            .await
            .expect("connect 2");
        wait_until("exit window disarmed", || {
            !manager.grace_armed.load(Ordering::Acquire)
        })
        .await;
        wait_until("reconnect counted", || manager.connection_count() == 1).await;
        assert!(
            !handle.is_finished(),
            "reconnect within the window must keep the daemon alive",
        );

        // Deliberate stop is untouched by the debounce: the shutdown token
        // exits immediately, grace window or not.
        drop(stream2);
        manager.shutdown_token().cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("accept_loop should exit within 5s")
            .expect("task should not panic");
        assert!(result.is_ok(), "accept_loop should return Ok");
    }

    /// The expiry re-check is authoritative (pulse-03): a census that
    /// recovered during the window aborts the exit at expiry (here the count
    /// is bumped directly, the same seam `stop_ack_reports_live_connection_
    /// count` uses, so no accept ran and no disarm raced the timer), and a
    /// later return to zero arms a fresh window that, unanswered, exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grace_expiry_recheck_spares_recovered_census() {
        let dir = tempfile::tempdir().expect("create tempdir");

        let manager = Arc::new(
            bind_in(dir.path()).disconnect_grace_override(std::time::Duration::from_millis(250)),
        );
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move { m.accept_loop().await });

        // Census at zero: arm the window.
        manager.disconnect.notify_one();
        wait_until("exit window armed", || {
            manager.grace_armed.load(Ordering::Acquire)
        })
        .await;

        // The census recovers during the window without an accept — the
        // count itself is what expiry re-checks.
        manager.connection_count.fetch_add(1, Ordering::Relaxed);

        // Expiry disarms without exiting.
        wait_until("expiry passed without exit", || {
            !manager.grace_armed.load(Ordering::Acquire)
        })
        .await;
        assert!(
            !handle.is_finished(),
            "a recovered census must survive window expiry",
        );

        // Back to zero: a fresh window arms and, unanswered, exits with the
        // last-client record.
        manager.connection_count.fetch_sub(1, Ordering::Relaxed);
        manager.disconnect.notify_one();
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

    #[test]
    fn project_config_nudges_fire_once_per_root() {
        // misc 202: the SessionStart project-config nudge is a doorbell — a root is
        // nudged on its first SessionStart (`mark` → `true`) and silent on every
        // later one (`false`), while a distinct root rings its own bell once. This
        // is the "second SessionStart same root → silent" acceptance leg.
        let ledger = ProjectConfigNudges::new();
        let root_a = PathBuf::from("/ws/project-a");
        let root_b = PathBuf::from("/ws/project-b");
        assert!(ledger.mark(&root_a), "first SessionStart on root-a nudges");
        assert!(
            !ledger.mark(&root_a),
            "second SessionStart on root-a is silent"
        );
        assert!(!ledger.mark(&root_a), "and every later one, too");
        assert!(
            ledger.mark(&root_b),
            "a different root rings its own bell once"
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

    // ── SessionStart auto-install dispatch (lsm 05) ──────────────────────

    const AUTO_SERVER: &str = "lsm05-router-ls";
    const AUTO_VERSION: &str = "1.0.0";
    const AUTO_MARKER: &str = "lsm05-router.marker";

    /// A config opted into auto-install with one marked language bound to
    /// [`AUTO_SERVER`].
    fn auto_install_config(auto_install: bool) -> crate::config::Config {
        let mut config = crate::config::Config::default_with_classification();
        config.servers = Some(crate::config::ServersConfig {
            prefer_managed: true,
            auto_install,
        });
        let mut lang = crate::config::LanguageConfig {
            root_markers: Some(vec![AUTO_MARKER.to_string()]),
            servers: Some(vec![crate::config::ServerBinding::new(AUTO_SERVER)]),
            ..crate::config::LanguageConfig::default()
        };
        lang.compile_markers().expect("plain marker compiles");
        config
            .language
            .insert("lsm05-router-lang".to_string(), lang);
        config
    }

    /// A manifest blessing [`AUTO_SERVER`] at [`AUTO_VERSION`] under a
    /// synthetic platform token (deterministic preferred-row fallback).
    fn auto_install_manifest() -> crate::recipes::BlessedManifest {
        let mut rows = std::collections::BTreeMap::new();
        rows.insert(
            "synthetic".to_string(),
            crate::recipes::BlessedEntry {
                version: AUTO_VERSION.to_string(),
                platform: "synthetic".to_string(),
                date: "2026-07-16".to_string(),
                tier: None,
            },
        );
        let mut blessed = std::collections::BTreeMap::new();
        blessed.insert(AUTO_SERVER.to_string(), rows);
        crate::recipes::BlessedManifest {
            blessed,
            ..crate::recipes::BlessedManifest::default()
        }
    }

    /// A cargo-class recipe map for [`AUTO_SERVER`] at [`AUTO_VERSION`].
    fn auto_install_recipes() -> std::collections::BTreeMap<String, crate::recipes::InstallRecipe> {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            AUTO_SERVER.to_string(),
            crate::recipes::InstallRecipe {
                ecosystem: crate::recipes::Ecosystem::Cargo,
                package: AUTO_SERVER.to_string(),
                version: AUTO_VERSION.to_string(),
                tier: crate::recipes::VerificationTier::CargoLocked,
                draft: false,
                hash: None,
                note: None,
                conformance: true,
                co_install: Vec::new(),
                artifact: std::collections::BTreeMap::new(),
                runtime: None,
            },
        );
        map
    }

    /// A stub install runner: reports success and stages the managed
    /// executable at the pin, standing in for `cargo install --root`.
    struct AutoStagingRunner {
        home_root: PathBuf,
        runs: Arc<AtomicUsize>,
    }

    impl crate::install::CommandRunner for AutoStagingRunner {
        fn run(
            &self,
            _command: &crate::install::InstallCommand,
        ) -> anyhow::Result<crate::install::CommandOutcome> {
            use std::os::unix::fs::PermissionsExt;

            self.runs.fetch_add(1, Ordering::SeqCst);
            let home = crate::managed_home::ManagedHome::at(self.home_root.clone());
            let bin = home.bin_dir(AUTO_SERVER, AUTO_VERSION).expect("bin dir");
            std::fs::create_dir_all(&bin).expect("mkdir");
            let exe = bin.join(AUTO_SERVER);
            std::fs::write(&exe, b"#!/bin/sh\n").expect("write");
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            Ok(crate::install::CommandOutcome {
                success: true,
                code: Some(0),
                output: String::new(),
            })
        }
    }

    /// A fetcher no cargo-class plan reaches.
    struct AutoNoFetch;
    impl crate::install::TarballFetcher for AutoNoFetch {
        fn fetch(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("unexpected fetch of {url}")
        }
    }

    /// The `session-start/clear-editing` request for a session rooted at `cwd`.
    fn session_start_request(cwd: &Path) -> serde_json::Value {
        serde_json::json!({
            "method": "session-start/clear-editing",
            "format": "claude",
            "session_id": "lsm05-session",
            "host_payload": {
                "cwd": cwd.display().to_string(),
                "source": "startup",
            },
        })
    }

    /// The `auto_install_announcement` field of a dispatch response, if any.
    fn announcement_of(response: &str) -> Option<String> {
        let trimmed = response.trim();
        if trimmed.is_empty() {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(trimmed)
            .ok()?
            .get("auto_install_announcement")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_start_kicks_background_auto_install_and_announces() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::write(root.join(AUTO_MARKER), b"").expect("write marker");
        let home_root = dir.path().join("servers");
        let runs = Arc::new(AtomicUsize::new(0));

        let installer = crate::auto_install::AutoInstaller::with_parts(
            crate::managed_home::ManagedHome::at(home_root.clone()),
            auto_install_recipes(),
            Some(Arc::new(auto_install_manifest())),
            Box::new(AutoStagingRunner {
                home_root: home_root.clone(),
                runs: runs.clone(),
            }),
            Box::new(AutoNoFetch),
            None,
        );

        let logging = LoggingServer::new();
        let session = Arc::new(crate::bridge::session::Session::new(
            auto_install_config(true),
            vec![],
            logging.clone(),
            "daemon".into(),
            tokio::runtime::Handle::current(),
            None,
        ));
        let config_path = dir
            .path()
            .join("config")
            .join("catenary")
            .join("config.toml");
        let manager = Arc::new(
            SessionManager::bind_at(&mcp_socket_in(dir.path()), &ipc_path, logging)
                .expect("bind")
                .config_path_override(config_path)
                .auto_installer_override(installer)
                .with_session(session),
        );
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // First SessionStart: the missing blessed server is detected and
        // kicked; the response carries the user-visible announcement. The
        // response arrives without waiting on the install (the dispatch is a
        // spawn) — the roundtrip completing at all while the install work runs
        // in the background is the wiring under test.
        let response = hook_roundtrip(&ipc_path, &session_start_request(&root)).await;
        let announcement =
            announcement_of(&response).expect("first session start announces the kick");
        assert!(
            announcement.contains(AUTO_SERVER) && announcement.contains(AUTO_VERSION),
            "the announcement names the server and pin: {announcement}",
        );
        assert!(
            announcement.contains("take minutes"),
            "a cargo (compile-class) recipe announces the minutes-class delay: {announcement}",
        );

        // The background install lands in the managed home at the pin.
        let home = crate::managed_home::ManagedHome::at(home_root.clone());
        let mut landed = false;
        for _ in 0..500 {
            if home
                .pinned_executable(AUTO_SERVER, AUTO_VERSION, AUTO_SERVER)
                .is_some()
            {
                landed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(landed, "the background install landed in the managed home");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "exactly one install ran");

        // A repeat SessionStart finds the server resolvable — detection is
        // silent, no re-kick, no re-announcement.
        let response = hook_roundtrip(&ipc_path, &session_start_request(&root)).await;
        assert_eq!(
            announcement_of(&response),
            None,
            "a landed install is no longer missing: {response}",
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no second install ran");

        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_start_auto_install_off_by_default_runs_nothing() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::write(root.join(AUTO_MARKER), b"").expect("write marker");
        let home_root = dir.path().join("servers");
        let runs = Arc::new(AtomicUsize::new(0));

        let installer = crate::auto_install::AutoInstaller::with_parts(
            crate::managed_home::ManagedHome::at(home_root.clone()),
            auto_install_recipes(),
            Some(Arc::new(auto_install_manifest())),
            Box::new(AutoStagingRunner {
                home_root: home_root.clone(),
                runs: runs.clone(),
            }),
            Box::new(AutoNoFetch),
            None,
        );

        let logging = LoggingServer::new();
        // The default: `[servers]` present but auto_install omitted/false.
        let session = Arc::new(crate::bridge::session::Session::new(
            auto_install_config(false),
            vec![],
            logging.clone(),
            "daemon".into(),
            tokio::runtime::Handle::current(),
            None,
        ));
        let config_path = dir
            .path()
            .join("config")
            .join("catenary")
            .join("config.toml");
        let manager = Arc::new(
            SessionManager::bind_at(&mcp_socket_in(dir.path()), &ipc_path, logging)
                .expect("bind")
                .config_path_override(config_path)
                .auto_installer_override(installer)
                .with_session(session),
        );
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let response = hook_roundtrip(&ipc_path, &session_start_request(&root)).await;
        assert_eq!(
            announcement_of(&response),
            None,
            "auto_install = false detects and announces nothing: {response}",
        );
        // Give any (buggy) background work a beat to surface, then assert
        // nothing ran at all — the acceptance's "detection runs nothing".
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "no install ran");

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
        bind_with_session_config(
            dir,
            roots,
            crate::config::Config::default_with_classification(),
        )
    }

    /// Like [`bind_with_session_roots`] but with an explicit config, so a test can
    /// inject a custom `[lsp.server.*]`/`[lsp.language.*]` binding (e.g. an
    /// unverified server — diagnostics-debt 04b).
    fn bind_with_session_config(
        dir: &Path,
        roots: Vec<PathBuf>,
        config: crate::config::Config,
    ) -> SessionManager {
        let logging = LoggingServer::new();
        let runtime = tokio::runtime::Handle::current();
        let instance_id: Arc<str> = "daemon".into();
        let session = Arc::new(crate::bridge::session::Session::new(
            config,
            roots,
            logging.clone(),
            instance_id,
            runtime,
            None,
        ));

        // Bug 109: pin the config-persistence path INTO this test's tempdir
        // before `with_session` resolves it. Every in-process hook-dispatch test
        // funnels through here, so a `tool/roots-add` that reaches `persist_pin`
        // writes `<dir>/config/catenary/config.toml` (inside the test's own
        // `TempDir`, cleaned on drop) instead of the maintainer's real
        // `~/.config/catenary/config.toml`. An in-process test cannot redirect
        // `config_dir()` via env (Rust 2024 forbids `std::env::set_var`), so the
        // override is the only leak-proof seam — and routing every test through it
        // makes the escape structurally unreachable, not merely fixed per-test.
        let config_path = dir.join("config").join("catenary").join("config.toml");
        SessionManager::bind_at(&mcp_socket_in(dir), &ipc_socket_in(dir), logging)
            .expect("bind")
            .config_path_override(config_path)
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
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default(),
            vec![],
            LoggingServer::new(),
            instance_id.clone(),
            tokio::runtime::Handle::current(),
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

        // The armed/paid axis reads the durable LEDGER now (bug 116); this
        // session has no real roots, so its ledger is always clear — an active
        // accumulator reads as `working`, never `editing`. The ledger-driven
        // editing↔working demotion is exercised against a real ledger in
        // `board_status_editing_working_rides_the_ledger` below.
        session
            .editing
            .start_editing(Some("sess-1"), "")
            .expect("start editing");
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
        // batch status (tui-rework 14, item 3): fresh → idle, and (with a clear
        // ledger) an active accumulator → working.
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
            true,
        );
        // No real ledger debt for this synthetic path → working, not editing.
        assert_eq!(
            board.sessions()[0].subagents[0].status,
            SessionStatus::Working
        );
    }

    /// The board's editing→working demotion rides the durable LEDGER, not the
    /// retired in-memory `delivered` flags (bug 116).
    ///
    /// Root-ownership stage 3 left nothing in production to pay the in-memory
    /// flags, so the board hung at `editing` from the first edit until a Stop.
    /// Both [`Session::status_in`] (session-wide, keyed on any root's ledger debt)
    /// and [`Session::subagent_status_in`] (per-agent candidate set intersected
    /// with the ledger) now read the touch-tree: booking a file arms `editing`,
    /// unlinking it (the delivery seam) demotes to `working`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn board_status_editing_working_rides_the_ledger() {
        use crate::state_snapshot::SessionStatus;

        // A real repo root and an isolated tempdir ledger base.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canon").join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("mk .git");
        std::fs::create_dir_all(root.join("src")).expect("mk src");
        let locks = dir.path().join("locks");
        let file = root.join("src/lib.rs");
        std::fs::write(&file, b"").expect("write file");

        let instance_id: Arc<str> = "sess-led".into();
        let session = Arc::new(crate::bridge::session::Session::new(
            crate::config::Config::default_with_classification(),
            vec![root.clone()],
            LoggingServer::new(),
            instance_id,
            tokio::runtime::Handle::current(),
            None,
        ));
        let owner = crate::lock::Owner::new("claude", "sess-led", "");
        let booking =
            crate::lock::Booking::from_config(&crate::config::Config::load().expect("cfg"));

        // ── Session-wide status ──────────────────────────────────────────
        // Active accumulator, empty ledger → working.
        session
            .editing
            .start_editing(Some("sess-led"), "")
            .expect("start");
        assert_eq!(session.status_in(&locks), SessionStatus::Working);

        // Book the edit into the real ledger → the gate is armed → editing.
        assert!(matches!(
            crate::lock::acquire_in(
                &locks,
                &file,
                &owner,
                &booking,
                std::time::SystemTime::now()
            ),
            crate::lock::Acquired::Ours
        ));
        session
            .editing
            .record_covered_edit(Some("sess-led"), "", file.clone(), true);
        assert_eq!(
            session.status_in(&locks),
            SessionStatus::Editing,
            "booked ledger debt arms the board's editing status"
        );

        // Delivery unlinks the touch file (the paid-diagnostics seam) → working.
        crate::lock::unlink_delivered_in(&locks, &root, std::slice::from_ref(&file));
        assert_eq!(
            session.status_in(&locks),
            SessionStatus::Working,
            "paying the ledger demotes editing→working — the dead demotion, revived"
        );

        // ── Per-agent subagent status ────────────────────────────────────
        session
            .editing
            .start_editing(Some("sess-led"), "agent-a")
            .expect("sub start");
        session
            .editing
            .record_covered_edit(Some("sess-led"), "agent-a", file.clone(), true);
        // Re-book (the delivery above cleared it) → the subagent's candidate is due.
        let _ = crate::lock::acquire_in(
            &locks,
            &file,
            &owner,
            &booking,
            std::time::SystemTime::now(),
        );
        assert_eq!(
            session.subagent_status_in("agent-a", &locks),
            SessionStatus::Editing,
            "a subagent candidate still due reads editing"
        );
        crate::lock::unlink_delivered_in(&locks, &root, std::slice::from_ref(&file));
        assert_eq!(
            session.subagent_status_in("agent-a", &locks),
            SessionStatus::Working,
            "paying the candidate demotes the subagent editing→working"
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

    /// Streams one single-hit batch through the `tool/hitstream` arm — the
    /// grep surface since the ws43-02 cutover — and returns the raw annotation
    /// frames. The query-side trigger for the mount tests: the annotator's
    /// auto-mount (and its sensitive-path gate) fires per batch, keyed on the
    /// batch's hit paths.
    async fn hitstream_annotate_one(ipc_path: &Path, file: &Path, text: &str) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stream = tokio::net::UnixStream::connect(ipc_path)
            .await
            .expect("connect to IPC socket");
        let (reader, mut writer) = stream.into_split();

        let hit = crate::hitstream::WireHit {
            path: file.to_path_buf(),
            line: 1,
            column: 1,
            text: text.to_string(),
        };
        let mut payload = serde_json::to_vec(&serde_json::json!({ "method": METHOD_HITSTREAM }))
            .expect("serialize handshake");
        payload.push(b'\n');
        payload.extend(
            serde_json::to_vec(&crate::hitstream::HitFrame::batch(0, vec![hit]))
                .expect("serialize batch"),
        );
        payload.push(b'\n');
        payload.extend(
            serde_json::to_vec(&crate::hitstream::HitFrame::end(1)).expect("serialize end"),
        );
        payload.push(b'\n');
        writer.write_all(&payload).await.expect("write hit frames");

        let mut buf_reader = BufReader::new(reader);
        let mut out = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = buf_reader.read_line(&mut line).await.expect("read frame");
            if n == 0 {
                break;
            }
            out.push_str(&line);
            if line.contains("\"frame\":\"end\"") {
                break;
            }
        }
        out
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

        // Session A enters editing mode and accumulates a covered file into its
        // in-memory batch. The batch is per-session (`EditingManager` keyed by
        // `(session_id, agent_id)`), so this must not appear under session B.
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
                true,
            );

            // The in-memory editing batch is per-session: session A holds the
            // file, session B holds nothing. (Root-ownership stage 3 moved the
            // diagnostics DEBT GATE off this in-memory batch onto the durable
            // ledger; the batch survives only for its non-gate roles — this
            // per-session isolation is one of them.)
            assert!(
                router_a.session.editing.has_files(Some("session-a"), ""),
                "session A's batch holds its covered file"
            );
            assert!(
                !router_a.session.editing.has_files(Some("session-b"), ""),
                "session B's batch is empty — editing state does not leak across sessions"
            );
        }

        // The Bash nag now reads the ledger (stage 3), not this in-memory batch.
        // A `pre-tool/editing-state` request carrying no `cwd` resolves to no
        // root, so the gate stands down and the non-filesystem Bash is allowed
        // (payability): with the daemon reachable but no resolvable kitchen there
        // is nothing to gate. Ledger-based gating is covered in
        // tests/root_lock_integration.rs.
        let req = serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Bash",
            "command": "cargo build",
            "agent_id": "",
            "session_id": "session-a"
        });
        let line = hook_roundtrip(&ipc_path, &req).await;
        assert_eq!(
            line.trim(),
            "",
            "the ledger gate stands down with no resolvable cwd (stage 3 payability)"
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

    // ── race_against_disconnect (bug 24 regression guards) ─────────
    //
    // Each test makes exactly ONE branch completable, so the outcome carries
    // no timing or scheduling assumption. `tokio::io::duplex` stands in for
    // the connection: dropping one end is the disconnect (EOF on the probe);
    // holding it open and silent starves the probe forever.

    /// bug 24: a gone client resolves the race as a disconnect. The pipeline
    /// is `pending()` — the probe's EOF read is the only completable branch.
    #[tokio::test]
    async fn race_against_disconnect_resolves_none_when_client_is_gone() {
        let (client, mut daemon_side) = tokio::io::duplex(8);
        drop(client);
        let outcome = race_against_disconnect(std::future::pending::<()>(), &mut daemon_side).await;
        assert!(
            outcome.is_none(),
            "a closed peer must resolve the race as a disconnect"
        );
    }

    /// bug 24: a completed pipeline resolves `Some` while the client holds the
    /// socket open and silent — the probe read is never completable.
    #[tokio::test]
    async fn race_against_disconnect_resolves_pipeline_outcome_when_client_holds() {
        let (_client, mut daemon_side) = tokio::io::duplex(8);
        let outcome = race_against_disconnect(std::future::ready(42), &mut daemon_side).await;
        assert_eq!(
            outcome,
            Some(42),
            "a ready pipeline must win while the client holds the socket open"
        );
    }

    /// bug 24: the losing pipeline future is dropped, not leaked — client gone
    /// means the daemon-side wait terminates.
    #[tokio::test]
    async fn race_against_disconnect_drops_the_losing_pipeline() {
        use std::sync::atomic::AtomicBool;

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let pipeline = async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        };

        let (client, mut daemon_side) = tokio::io::duplex(8);
        drop(client);
        let outcome = race_against_disconnect(pipeline, &mut daemon_side).await;
        assert!(outcome.is_none(), "the disconnect arm must win");
        assert!(
            dropped.load(Ordering::SeqCst),
            "the losing pipeline future must be dropped on disconnect"
        );
    }

    // ── Keyed handoff structure tests (ADR 014; re-seated on Claim in ────
    // root-ownership stage 3 — the Diagnostics key/payload demolished with the
    // identity-correlation handoff, Claim the sole surviving transport).

    /// The same key serializes in order: a second `claim` acquire blocks until
    /// the first permit is released (no overwrite, no double-consume).
    #[tokio::test]
    async fn keyed_handoff_same_key_serializes() {
        let handoff = KeyedHandoff::new();

        let first = handoff
            .acquire(HandoffKey::Claim)
            .await
            .expect("first acquire");

        // A second same-key acquire blocks while the first permit is held —
        // the timeout elapses rather than completing.
        let blocked = tokio::time::timeout(
            Duration::from_millis(200),
            handoff.acquire(HandoffKey::Claim),
        )
        .await;
        assert!(
            blocked.is_err(),
            "second claim acquire must block while the first is held",
        );

        // Releasing the first lets the second proceed.
        drop(first);
        let second =
            tokio::time::timeout(Duration::from_secs(1), handoff.acquire(HandoffKey::Claim)).await;
        assert!(
            second.is_ok(),
            "second claim acquire must proceed once the first is released",
        );
    }

    /// Stage → consume round-trips the payload, frees the permit, and a second
    /// consume sees the empty slot.
    #[tokio::test]
    async fn keyed_handoff_stage_consume_roundtrip() {
        let handoff = KeyedHandoff::new();

        let permit = handoff.acquire(HandoffKey::Claim).await.expect("acquire");
        handoff.stage(
            HandoffKey::Claim,
            HandoffContext {
                payload: HandoffPayload::Claim {
                    answer: "claimed /repo (previous editor last seen 3m ago)".to_string(),
                },
                permit,
            },
        );

        let consumed = handoff
            .consume(HandoffKey::Claim)
            .expect("consume staged context");
        let HandoffPayload::Claim { answer } = &consumed.payload;
        assert_eq!(answer, "claimed /repo (previous editor last seen 3m ago)");

        // Slot is now empty — a second consume yields None.
        assert!(
            handoff.consume(HandoffKey::Claim).is_none(),
            "double consume must yield None",
        );

        // Dropping the consumed context frees the permit for the next stage.
        drop(consumed);
        let reacquire =
            tokio::time::timeout(Duration::from_secs(1), handoff.acquire(HandoffKey::Claim)).await;
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

        let permit = handoff.acquire(HandoffKey::Claim).await.expect("acquire");
        handoff.stage(
            HandoffKey::Claim,
            HandoffContext {
                payload: HandoffPayload::Claim {
                    answer: "claimed /x".to_string(),
                },
                permit,
            },
        );

        // Wait past the (short, test-only) per-key timeout; the spawned task
        // clears the slot.
        tokio::time::sleep(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;

        assert!(
            handoff.consume(HandoffKey::Claim).is_none(),
            "claim stage must be cleared after its timeout",
        );

        // The permit was released on timeout — a fresh acquire proceeds.
        let _claim = handoff
            .acquire(HandoffKey::Claim)
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

    /// Spawn a fake daemon thread that reads an initialize request, then the
    /// bridge-hello notification, and responds with `serverInfo.version` set to
    /// `daemon_version`. Captures the hello's `bridgeVersion` (as a JSON string
    /// value) into `captured` so a test can assert the wire carried the bridge's
    /// version — the string is `"<absent>"` if no hello with that field arrived.
    ///
    /// Post-ws41-02 the bridge no longer compares the daemon's
    /// `serverInfo.version` — the daemon compares the bridge's hello — so
    /// `daemon_version` here only exercises that the response is forwarded
    /// verbatim regardless of what the daemon reports.
    fn fake_daemon(
        stream: std::os::unix::net::UnixStream,
        daemon_version: &str,
        captured: std::sync::Arc<std::sync::Mutex<String>>,
    ) -> std::thread::JoinHandle<()> {
        let daemon_version = daemon_version.to_string();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut reader = std::io::BufReader::new(&stream);

            // 1) initialize request
            let mut init = String::new();
            reader.read_line(&mut init).expect("read init request");

            // 2) bridge-hello notification (ws41-02) — capture its bridgeVersion.
            let mut hello = String::new();
            reader.read_line(&mut hello).expect("read bridge hello");
            let parsed: serde_json::Value =
                serde_json::from_str(hello.trim()).expect("parse hello");
            let method = parsed["method"].as_str().unwrap_or_default();
            if method == catenary_mcp::protocol::BRIDGE_HELLO_METHOD {
                let bridge_version = parsed
                    .pointer("/params/bridgeVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<absent>");
                *captured.lock().expect("lock") = bridge_version.to_string();
            }

            // 3) initialize response
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {
                        "name": "catenary",
                        "version": daemon_version,
                    }
                }
            });
            let mut w: &std::os::unix::net::UnixStream = &stream;
            writeln!(w, "{}", serde_json::to_string(&response).expect("ser"))
                .expect("write response");
        })
    }

    #[test]
    fn handshake_sends_bridge_version_and_forwards_response() {
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let handle = fake_daemon(server_sock, catenary_mcp::version(), captured.clone());

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        version_handshake(&mut stdin, &client_sock, &mut stdout)
            .expect("handshake forwards the initialize response and returns the init line");
        handle.join().expect("daemon thread");

        // The wire carried the bridge's compiled catenary-mcp version.
        assert_eq!(
            captured.lock().expect("lock").as_str(),
            catenary_mcp::version(),
            "the bridge hello must carry the bridge's catenary-mcp version",
        );

        // The daemon's initialize response is forwarded to the client verbatim.
        assert!(!stdout.is_empty(), "response should be forwarded to stdout");
        let response: serde_json::Value =
            serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim())
                .expect("parse response");
        assert_eq!(response["result"]["serverInfo"]["name"], "catenary");
    }

    #[test]
    fn handshake_never_bails_on_daemon_version_disagreement() {
        // A daemon reporting a different serverInfo.version no longer tears the
        // bridge down (ws41-02): comparison and surfacing are daemon-side; the
        // bridge forwards the response and connects regardless.
        let (server_sock, client_sock) =
            std::os::unix::net::UnixStream::pair().expect("stream pair");

        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let handle = fake_daemon(server_sock, "0.0.0-fake", captured.clone());

        let mut stdin = std::io::Cursor::new(init_request_line());
        let mut stdout = Vec::new();

        version_handshake(&mut stdin, &client_sock, &mut stdout)
            .expect("a running bridge survives a version disagreement");
        handle.join().expect("daemon thread");

        assert!(
            !stdout.is_empty(),
            "the response is forwarded even when the daemon's version differs",
        );
        assert_eq!(
            captured.lock().expect("lock").as_str(),
            catenary_mcp::version(),
            "the hello still carried the bridge version",
        );
    }

    // ── Bridge-hello surfacing tests (ws41-02) ───────────────────────────

    #[test]
    fn handle_bridge_hello_matching_version_records_nothing() {
        use crate::state_snapshot::{DaemonInfo, SnapshotWriter, now_iso};

        let rt = tokio::runtime::Runtime::new().expect("rt");
        let _guard = rt.enter();
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::new(
            rt.handle(),
            dir.path(),
            DaemonInfo::current("daemon:test".to_string(), 1, now_iso()),
        );
        let dedup = Arc::new(std::sync::Mutex::new(HashSet::new()));

        handle_bridge_hello(Some(catenary_mcp::version()), Some(&writer), &dedup);

        assert!(
            dedup.lock().expect("lock").is_empty(),
            "a matching version fires no interrupt",
        );
    }

    #[test]
    fn handle_bridge_hello_mismatch_fires_once_per_pairing() {
        use crate::state_snapshot::{DaemonInfo, SnapshotWriter, now_iso};

        let rt = tokio::runtime::Runtime::new().expect("rt");
        let _guard = rt.enter();
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = SnapshotWriter::new(
            rt.handle(),
            dir.path(),
            DaemonInfo::current("daemon:test".to_string(), 1, now_iso()),
        );
        let dedup = Arc::new(std::sync::Mutex::new(HashSet::new()));

        // Same mismatched pairing observed three times across "session starts".
        for _ in 0..3 {
            handle_bridge_hello(Some("0.0.0-older"), Some(&writer), &dedup);
        }
        assert_eq!(
            dedup.lock().expect("lock").len(),
            1,
            "the interrupt dedups to one entry per (bridge, daemon) pairing",
        );

        // A pre-handshake bridge (no version) is a distinct pairing → a second
        // interrupt entry, but still only once for that pairing.
        for _ in 0..2 {
            handle_bridge_hello(None, Some(&writer), &dedup);
        }
        assert_eq!(
            dedup.lock().expect("lock").len(),
            2,
            "a new (pre-handshake, daemon) pairing fires its own single interrupt",
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

    // ── The answer desk (misc 201, decision 031) ────────────────────────

    /// A `PermissionRequest` `host_payload` fixture for a read-class tool.
    fn read_payload(tool: &str, path_key: &str, path: &str, cwd: &str) -> serde_json::Value {
        serde_json::json!({
            "tool_name": tool,
            "tool_input": { path_key: path },
            "cwd": cwd,
        })
    }

    #[test]
    fn permission_read_target_only_reads_class_and_resolves_relative() {
        // Read → file_path; Grep/Glob → path; write-class → None.
        let read = read_payload("Read", "file_path", "/abs/main.rs", "/work");
        let (tool, target) = permission_read_target(&read).expect("read is read-class");
        assert_eq!(tool, "Read");
        assert_eq!(target, PathBuf::from("/abs/main.rs"));

        // A relative path resolves against cwd.
        let rel = read_payload("Read", "file_path", "src/main.rs", "/work");
        let (_, target) = permission_read_target(&rel).expect("relative resolves");
        assert_eq!(target, PathBuf::from("/work/src/main.rs"));

        let grep = read_payload("Grep", "path", "/abs", "/work");
        assert_eq!(permission_read_target(&grep).expect("grep").0, "Grep");

        // Write-class → the desk answers nothing.
        let write = read_payload("Write", "file_path", "/abs/x.rs", "/work");
        assert!(permission_read_target(&write).is_none());
        let edit = read_payload("Edit", "file_path", "/abs/x.rs", "/work");
        assert!(permission_read_target(&edit).is_none());
    }

    #[test]
    fn answer_desk_denies_sensitive_and_quiet_allows_in_scope() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("write");
        std::fs::write(root.join(".env"), "SECRET=1").expect("write env");

        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![root.canonicalize().expect("canon")]);
        let config = crate::config::Config::default();
        let promoted = PromotedPrefixes::new();

        // In-scope file → quiet allow, realpath pinned.
        let read = read_payload(
            "Read",
            "file_path",
            root.join("src/main.rs").to_str().expect("utf8"),
            tmp.path().to_str().expect("utf8"),
        );
        let (decision, tool) =
            resolve_permission_decision(&read, Some(&tracker), &config, &promoted, "sess")
                .expect("read decided");
        assert_eq!(tool, "Read");
        assert!(matches!(
            decision,
            crate::answer_desk::Decision::QuietAllow { .. }
        ));

        // A `.env` inside scope is still sensitive → deny.
        let env = read_payload(
            "Read",
            "file_path",
            root.join(".env").to_str().expect("utf8"),
            tmp.path().to_str().expect("utf8"),
        );
        let (decision, _) =
            resolve_permission_decision(&env, Some(&tracker), &config, &promoted, "sess")
                .expect("env decided");
        assert!(matches!(
            decision,
            crate::answer_desk::Decision::Deny { .. }
        ));
    }

    /// Mount state never converts into a desk answer (decision 031): a root held
    /// ONLY by an `ephemeral:*` contributor (a query automount) is excluded from
    /// the declared scope, so reads under it are LOUD allows, not quiet ones —
    /// an agent must not self-grant quiet scope by grepping a path. The same
    /// root gains scope the moment a genuine session contributor holds it too.
    #[test]
    fn answer_desk_scope_excludes_ephemeral_only_roots() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canon").join("repo");
        std::fs::create_dir_all(&root).expect("mk root");
        let file = root.join("notes.txt");
        std::fs::write(&file, "hi").expect("write");

        let tracker = RootTracker::new();
        tracker.add_roots(&ephemeral_contributor(&root), std::slice::from_ref(&root));
        let config = crate::config::Config::default();
        let promoted = PromotedPrefixes::new();

        let read = read_payload(
            "Read",
            "file_path",
            file.to_str().expect("utf8"),
            root.to_str().expect("utf8"),
        );
        let (decision, _) =
            resolve_permission_decision(&read, Some(&tracker), &config, &promoted, "sess")
                .expect("read decided");
        assert!(
            matches!(decision, crate::answer_desk::Decision::LoudAllow { .. }),
            "an ephemeral-only root confers no quiet scope, got {decision:?}"
        );

        // A genuine session contribution on the same root flips it to quiet.
        tracker.add_roots("mcp:1", std::slice::from_ref(&root));
        let (decision, _) =
            resolve_permission_decision(&read, Some(&tracker), &config, &promoted, "sess")
                .expect("read decided");
        assert!(
            matches!(decision, crate::answer_desk::Decision::QuietAllow { .. }),
            "a session-contributed root is declared scope, got {decision:?}"
        );
    }

    #[test]
    fn answer_desk_loud_allow_out_of_scope() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "hi").expect("write");

        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![PathBuf::from("/some/other/root")]);
        let config = crate::config::Config::default();
        let promoted = PromotedPrefixes::new();

        let read = read_payload(
            "Read",
            "file_path",
            outside.to_str().expect("utf8"),
            "/some/other/root",
        );
        let (decision, _) =
            resolve_permission_decision(&read, Some(&tracker), &config, &promoted, "sess")
                .expect("read decided");
        assert!(matches!(
            decision,
            crate::answer_desk::Decision::LoudAllow { .. }
        ));
    }

    #[test]
    fn answer_desk_write_class_emits_no_decision() {
        let tracker = RootTracker::new();
        let config = crate::config::Config::default();
        let promoted = PromotedPrefixes::new();
        let write = read_payload("Write", "file_path", "/work/x.rs", "/work");
        assert!(
            resolve_permission_decision(&write, Some(&tracker), &config, &promoted, "sess")
                .is_none()
        );
    }

    #[test]
    fn answer_desk_always_read_promotes_only_on_first_allow() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let docs = tmp.path().join("docs");
        std::fs::create_dir_all(&docs).expect("mkdir docs");
        std::fs::write(docs.join("a.md"), "a").expect("write a");
        std::fs::write(docs.join("b.md"), "b").expect("write b");
        let docs_canon = docs.canonicalize().expect("canon");

        // always_read carries the docs prefix; no workspace roots.
        let mut config = crate::config::Config::default();
        config.permissions = Some(crate::config::PermissionsConfig {
            always_read: vec![docs_canon.to_string_lossy().into_owned()],
            ..Default::default()
        });
        let tracker = RootTracker::new();
        let promoted = PromotedPrefixes::new();

        let first = read_payload(
            "Read",
            "file_path",
            docs.join("a.md").to_str().expect("utf8"),
            tmp.path().to_str().expect("utf8"),
        );
        let (decision, _) =
            resolve_permission_decision(&first, Some(&tracker), &config, &promoted, "sess")
                .expect("first decided");
        match decision {
            crate::answer_desk::Decision::AlwaysReadAllow { promote, .. } => {
                assert!(promote, "first allow under always_read promotes");
            }
            other => panic!("expected AlwaysReadAllow, got {other:?}"),
        }

        // A second read under the same prefix in the same session does NOT promote.
        let second = read_payload(
            "Read",
            "file_path",
            docs.join("b.md").to_str().expect("utf8"),
            tmp.path().to_str().expect("utf8"),
        );
        let (decision, _) =
            resolve_permission_decision(&second, Some(&tracker), &config, &promoted, "sess")
                .expect("second decided");
        match decision {
            crate::answer_desk::Decision::AlwaysReadAllow { promote, .. } => {
                assert!(!promote, "subsequent allow does not re-promote");
            }
            other => panic!("expected AlwaysReadAllow, got {other:?}"),
        }
    }

    #[test]
    fn answer_desk_scope_matches_through_symlinked_prefix() {
        // The spelling rule: a read spelled through a symlinked prefix alias must
        // land in-scope, because the target canonicalizes to the same realpath the
        // declared (canonical) root covers.
        let tmp = tempfile::tempdir().expect("tempdir");
        let canonical_root = tmp.path().join("real-repo");
        std::fs::create_dir_all(canonical_root.join("src")).expect("mkdir");
        std::fs::write(canonical_root.join("src/main.rs"), "fn main() {}").expect("write");
        let canonical_root = canonical_root.canonicalize().expect("canon");
        let link = tmp.path().join("alias-repo");
        std::os::unix::fs::symlink(&canonical_root, &link).expect("symlink");

        let tracker = RootTracker::new();
        tracker.set_roots("mcp:1", vec![canonical_root]);
        let config = crate::config::Config::default();
        let promoted = PromotedPrefixes::new();

        // Read spelled through the ALIAS prefix.
        let read = read_payload(
            "Read",
            "file_path",
            link.join("src/main.rs").to_str().expect("utf8"),
            tmp.path().to_str().expect("utf8"),
        );
        let (decision, _) =
            resolve_permission_decision(&read, Some(&tracker), &config, &promoted, "sess")
                .expect("read decided");
        assert!(
            matches!(decision, crate::answer_desk::Decision::QuietAllow { .. }),
            "an aliased read lands in-scope after canonicalization",
        );
    }

    // ── Read-action recorder (misc 201, "record ALL reads") ─────────────

    /// A `pre-tool/editing-state` request fixture whose `host_payload` names a
    /// read-class tool — the daemon-side shape `record_read_action` reads.
    fn pre_tool_read_request(
        tool: &str,
        path_key: &str,
        path: &str,
        cwd: &str,
        agent_id: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "method": "pre-tool/editing-state",
            "tool_name": tool,
            "agent_id": agent_id,
            "session_id": "sess-1",
            "cwd": cwd,
            "host_payload": {
                "tool_name": tool,
                "cwd": cwd,
                "tool_input": {
                    path_key: path,
                    // A content-bearing field the recorder must NEVER echo.
                    "CONTENT_MARKER": "secret-file-content-should-never-be-recorded",
                },
            },
        })
    }

    /// The `read action` rows the recorder captured (internal-kind, info-level).
    fn read_action_rows(
        recorder: &Arc<crate::logging::test_support::MessageRecorder>,
    ) -> Vec<crate::logging::test_support::MsgRow> {
        crate::logging::test_support::query_all_messages(recorder)
            .into_iter()
            .filter(|m| m.payload.contains("\"message\":\"read action\""))
            .collect()
    }

    #[test]
    fn read_action_records_one_event_with_expected_fields() {
        let (_logging, recorder, _guard) = crate::logging::test_support::setup_logging();

        let req = pre_tool_read_request("Read", "file_path", "/abs/main.rs", "/work", "agent-7");
        record_read_action(&req, "sess-1");

        let rows = read_action_rows(&recorder);
        assert_eq!(rows.len(), 1, "exactly one read-action record");
        let row = &rows[0];
        assert_eq!(
            row.r#type, "internal",
            "an internal trace event, not a hook row"
        );
        assert_eq!(row.level, "info", "info-level by ruling — firehose only");
        // The action fields — tool, path, agent, cwd — land in the trace payload;
        // never content. (`session_id` is a reserved firehose column pulled out of
        // the payload projection, so it is carried but not asserted on here.)
        assert!(row.payload.contains("\"source\":\"hook.dispatch\""));
        assert!(row.payload.contains("\"tool\":\"Read\""));
        assert!(row.payload.contains("/abs/main.rs"), "target path recorded");
        assert!(row.payload.contains("agent-7"), "agent id recorded");
        assert!(row.payload.contains("/work"), "cwd recorded");
    }

    #[test]
    fn read_action_records_grep_and_glob() {
        let (_logging, recorder, _guard) = crate::logging::test_support::setup_logging();

        // Grep/Glob name their target under `path`, not `file_path`.
        let grep = pre_tool_read_request("Grep", "path", "/repo/src", "/repo", "");
        record_read_action(&grep, "sess-1");
        let glob = pre_tool_read_request("Glob", "path", "/repo", "/repo", "");
        record_read_action(&glob, "sess-1");

        let rows = read_action_rows(&recorder);
        assert_eq!(rows.len(), 2, "one record per read-class dispatch");
        assert!(rows.iter().any(|r| r.payload.contains("\"tool\":\"Grep\"")));
        assert!(rows.iter().any(|r| r.payload.contains("\"tool\":\"Glob\"")));
    }

    #[test]
    fn read_action_ignores_write_class_tools() {
        let (_logging, recorder, _guard) = crate::logging::test_support::setup_logging();

        // A write-class tool is not read-class → the recorder records nothing.
        let write = pre_tool_read_request("Write", "file_path", "/abs/x.rs", "/work", "");
        record_read_action(&write, "sess-1");
        let edit = pre_tool_read_request("Edit", "file_path", "/abs/x.rs", "/work", "");
        record_read_action(&edit, "sess-1");

        assert!(
            read_action_rows(&recorder).is_empty(),
            "write-class tools produce no read-action record",
        );
    }

    #[test]
    fn read_action_never_records_file_content() {
        let (_logging, recorder, _guard) = crate::logging::test_support::setup_logging();

        // The fixture's tool_input carries a content marker; the record must
        // carry the ACTION only, never any content that rode the payload.
        let req = pre_tool_read_request("Read", "file_path", "/abs/main.rs", "/work", "a");
        record_read_action(&req, "sess-1");

        let rows = read_action_rows(&recorder);
        assert_eq!(rows.len(), 1, "one record");
        assert!(
            !rows[0]
                .payload
                .contains("secret-file-content-should-never-be-recorded"),
            "no file content ever appears in the read-action record",
        );
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
    fn contributors_of_root_finds_every_holder_across_prefixes() {
        // Bug 93: a landed worktree can be held by its `worktree:` mount AND an
        // `ephemeral:`/`hook`/`mcp:` contributor at once. Full retirement needs
        // every holder regardless of key prefix — the narrow `worktree:`-only
        // reap left the root alive under the other keys.
        let tracker = RootTracker::new();
        let wt = PathBuf::from("/wt/landed");
        tracker.set_roots("worktree:sess:/wt/landed", vec![wt.clone()]);
        tracker.set_roots("ephemeral:/wt/landed", vec![wt.clone()]);
        tracker.set_roots("hook", vec![wt.clone(), PathBuf::from("/other")]);
        tracker.set_roots("mcp:7", vec![PathBuf::from("/other")]);

        let mut holders = tracker.contributors_of_root(&wt);
        holders.sort();
        assert_eq!(
            holders,
            vec![
                "ephemeral:/wt/landed".to_string(),
                "hook".to_string(),
                "worktree:sess:/wt/landed".to_string(),
            ],
            "every contributor declaring the landed path is returned, whatever its prefix",
        );

        // A path held by nobody yields an empty holder set.
        assert!(
            tracker
                .contributors_of_root(Path::new("/never/tracked"))
                .is_empty(),
            "an untracked path has no holders",
        );

        // Retirement releases exactly this root from every holder (`remove_root`,
        // never `remove_contributor`): the union drops it, while a multi-root
        // contributor keeps its OTHER roots — an `mcp:` connection declaring the
        // primary repo must not lose it when a worktree it also declared lands.
        for holder in holders {
            assert!(tracker.remove_root(&holder, &wt));
        }
        assert_eq!(
            tracker.refcount(&wt),
            0,
            "the retired root leaves the union once every holder lets go",
        );
        assert!(
            tracker
                .contributors_of_root(Path::new("/other"))
                .contains(&"hook".to_string()),
            "a multi-root holder keeps its other roots after the retirement",
        );
        assert!(
            tracker.global_roots().contains(&PathBuf::from("/other")),
            "an unrelated root survives the retirement",
        );
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

    #[test]
    fn merged_and_linger_oracles_share_the_once_ledger() {
        // wf-04 dedupe: the merged-linger advisory marks the SAME `nagged`
        // ledger as the owner-dead/root-unmounted nag, so a worktree
        // qualifying under both oracles draws exactly one line — whichever
        // oracle marks first wins the shot.
        let registry = WorktreeRegistry::new();
        let wt = PathBuf::from("/state/catenary/worktrees/agents/s/b");
        assert!(registry.mark_nagged(&wt), "the first oracle draws the line");
        assert!(
            !registry.mark_nagged(&wt),
            "the second oracle is deduped for the same worktree",
        );
    }

    #[test]
    fn is_main_agent_stop_requires_top_level_stop_and_no_agent_id() {
        // wf-04: the merged-linger nag aims at the MAIN agent — in Claude Code
        // the identity with NO agentId. A subagent (any identity WITH one)
        // never draws it, and SubagentStop is excluded outright.
        let main_stop = serde_json::json!({
            "agent_id": "",
            "host_payload": { "hook_event_name": "Stop" },
        });
        assert!(
            is_main_agent_stop(&main_stop),
            "top-level Stop, no agent id"
        );

        let subagent_identity = serde_json::json!({
            "agent_id": "abc123",
            "host_payload": { "hook_event_name": "Stop" },
        });
        assert!(
            !is_main_agent_stop(&subagent_identity),
            "an identity WITH an agentId gets no nag",
        );

        let subagent_stop = serde_json::json!({
            "agent_id": "",
            "host_payload": { "hook_event_name": "SubagentStop" },
        });
        assert!(!is_main_agent_stop(&subagent_stop), "SubagentStop excluded");

        let absent_agent_id = serde_json::json!({
            "host_payload": { "hook_event_name": "Stop" },
        });
        assert!(
            is_main_agent_stop(&absent_agent_id),
            "an absent agent_id field is the main identity too",
        );
    }

    #[test]
    fn merged_nudge_message_names_count_and_paths() {
        let one = merged_nudge_message(&[PathBuf::from("/wt/a")]);
        assert!(
            one.starts_with("1 worktree is already merged into main; `catenary worktree rm` it:"),
            "singular header: {one}"
        );
        assert!(one.contains("/wt/a"), "path listed: {one}");

        let two = merged_nudge_message(&[PathBuf::from("/wt/a"), PathBuf::from("/wt/b")]);
        assert!(
            two.starts_with(
                "2 worktrees are already merged into main; `catenary worktree rm` them:"
            ),
            "plural header: {two}"
        );
        assert!(
            two.contains("/wt/a") && two.contains("/wt/b"),
            "paths listed: {two}"
        );
    }

    #[test]
    fn worktree_registry_mark_surfaced_is_once_per_worktree() {
        // The dirty-kept surfacing fires once per worktree path per daemon
        // lifetime (bug 91) — the dedup that collapses the phantom-agent pileup.
        let registry = WorktreeRegistry::new();
        let wt = PathBuf::from("/state/catenary/worktrees/agents/s/a");
        assert!(registry.mark_surfaced(&wt), "first surfacing marks");
        assert!(
            !registry.mark_surfaced(&wt),
            "second surfacing is suppressed"
        );
    }

    #[test]
    fn surface_dirty_kept_is_once_per_path() {
        // Bug 91 — the sighting pinned: several subagents whose cwd resolves to
        // the ONE surviving dirty worktree each drive `surface_dirty_kept` for
        // that same path (the phantom-agent pileup). It must record exactly ONCE
        // per worktree path (root-ownership 04: a firehose/TUI record, no longer a
        // parent-agent queue). The once-per-path dedup rides
        // `WorktreeRegistry::mark_surfaced`.
        let registry = WorktreeRegistry::new();
        // The surviving dirty worktree — its leaf dir name IS the owner agent id
        // (misc 150), matching the ticket's `.../a8458e9e8f03be469`.
        let worktree = PathBuf::from("/state/catenary/worktrees/agents/sess-1/a8458e9e8f03be469");

        // The first call records and marks the path surfaced.
        surface_dirty_kept(&registry, "sess-1", &worktree);
        // Three phantom stops + the real one all resolve (via cwd) to this path;
        // every subsequent call is a no-op — the path is already marked surfaced,
        // so `mark_surfaced` now returns `false` for it.
        assert!(
            !registry.mark_surfaced(&worktree),
            "the first surface_dirty_kept marked the path surfaced (deduped)",
        );
    }

    #[test]
    fn worktree_registry_prune_missing_ages_out_gone_entries_and_clears_marks() {
        // Bug 91 age-out: a registration whose worktree dir is gone is dropped,
        // and its once-per-worktree nag/surfaced marks are cleared with it, so a
        // path recreated there is not denied a fresh reminder.
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");

        let present = base.join("present");
        std::fs::create_dir(&present).expect("mkdir present");
        let gone = base.join("gone");
        std::fs::create_dir(&gone).expect("mkdir gone");

        let registry = WorktreeRegistry::new();
        for (wt, agent) in [(&present, "present"), (&gone, "gone")] {
            registry.register(crate::worktree_create::WorktreeMeta {
                worktree: wt.clone(),
                source_repo: PathBuf::from("/repo"),
                base_commit: "deadbeef".to_string(),
                branch: format!("agent-{agent}"),
                name: format!("agent-{agent}"),
                agent_id: Some(agent.to_string()),
                session_id: "sess-1".to_string(),
                created_at: "2026-07-06T00:00:00.000Z".to_string(),
                class: "agent".to_string(),
                link: None,
                vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
            });
        }
        // Mark both surfaced/nagged, then delete the `gone` dir.
        assert!(registry.mark_surfaced(&gone));
        assert!(registry.mark_nagged(&gone));
        assert!(registry.mark_surfaced(&present));
        std::fs::remove_dir_all(&gone).expect("rm gone");

        let pruned = registry.prune_missing();
        assert_eq!(pruned, vec![gone.clone()], "only the gone dir is pruned");
        assert_eq!(registry.len(), 1, "the present registration survives");
        assert_eq!(
            registry.get(&present).map(|m| m.worktree),
            Some(present.clone()),
            "the present worktree is still registered",
        );
        // The gone path's marks cleared → a worktree recreated there re-surfaces.
        assert!(
            registry.mark_surfaced(&gone),
            "the pruned path's surfaced mark was cleared (re-surface allowed)",
        );
        assert!(
            registry.mark_nagged(&gone),
            "the pruned path's nagged mark was cleared (re-nag allowed)",
        );
        // The present path is untouched — its mark still suppresses a re-surface.
        assert!(
            !registry.mark_surfaced(&present),
            "the surviving worktree's surfaced mark is preserved (still deduped)",
        );
    }

    #[test]
    fn surface_dirty_kept_always_reports_a_present_dirty_worktree() {
        // The safety invariant (bug 91): dedup must never suppress the FIRST
        // report of a worktree that exists and is dirty. After prune ages out a
        // gone registration (clearing its mark), a worktree recreated at the same
        // path surfaces afresh — the safety net never goes silent for a present,
        // dirty worktree.
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let worktree = base.join("agents").join("sess-1").join("owner-id");
        std::fs::create_dir_all(&worktree).expect("mkdir worktree");

        let registry = WorktreeRegistry::new();
        registry.register(crate::worktree_create::WorktreeMeta {
            worktree: worktree.clone(),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "agent-owner-id".to_string(),
            name: "agent-owner-id".to_string(),
            agent_id: Some("owner-id".to_string()),
            session_id: "sess-1".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: "agent".to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        });

        // First surfacing — always recorded (marks the path surfaced).
        surface_dirty_kept(&registry, "sess-1", &worktree);
        // Duplicate while the dir still exists — suppressed (already marked).
        assert!(
            !registry.mark_surfaced(&worktree),
            "a duplicate for the same present path is deduped",
        );

        // The dir is landed away → the age-out prune drops the registration and
        // clears its surfaced mark.
        std::fs::remove_dir_all(&worktree).expect("rm worktree");
        assert_eq!(
            registry.prune_missing(),
            vec![worktree.clone()],
            "the gone registration ages out",
        );
        // A new worktree is created at the same path — it must surface afresh.
        std::fs::create_dir_all(&worktree).expect("recreate worktree");
        surface_dirty_kept(&registry, "sess-1", &worktree);
        assert!(
            !registry.mark_surfaced(&worktree),
            "a worktree recreated at a pruned path surfaces afresh then re-dedups \
             (safety net intact)",
        );
    }

    // ── SubagentStop dispose identity gate (bug 103) ─────────────────────
    //
    // The cwd-enclosing fallback in `resolve_stop_reap_target` can surface a live
    // sibling's worktree for a nested/foreign subagent's stop; the clean arm then
    // *deleted* that live tree. The gate: a stop disposes only its own worktree,
    // owned by the tree's on-disk dirname (bug 91's dirname-IS-owner primitive).

    #[test]
    fn stop_owns_worktree_gates_dispose_by_owner_dirname() {
        // The gate decision in isolation (bug 103). The worktree leaf dir name IS
        // the owner agent id (misc 150: `worktree_segment` uses the bare
        // `agent-<id>` id as the segment).
        let worktree = PathBuf::from("/state/catenary/worktrees/agents/sess-1/a8458e9e8f03be469");

        // Matching owner → the tree's own stop disposes.
        assert!(
            stop_owns_worktree("a8458e9e8f03be469", &worktree),
            "the owning agent's own stop may dispose its worktree",
        );
        // A nested/foreign subagent whose cwd merely encloses this tree → declined.
        assert!(
            !stop_owns_worktree("nested-guide-agent", &worktree),
            "a foreign/nested stop must NOT select this live sibling for disposal",
        );
        // An empty id (no agent identity in the stop payload) can never prove
        // ownership — declined, never a cwd-selected delete.
        assert!(
            !stop_owns_worktree("", &worktree),
            "an empty stop identity never owns a worktree",
        );
    }

    #[test]
    fn dispose_background_foreign_stop_never_disposes_and_keeps_registry() {
        // Bug 103 wiring: a foreign/nested stop drives
        // `dispose_worktree_in_background` for a live sibling's tree (the cwd
        // fallback surfaced it). The gate must short-circuit BEFORE `dispose`, so
        // the registration survives, the directory survives, and nothing is
        // surfaced — the exact bug (a live clean tree deleted) does not happen.
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        // The owner's live worktree: its leaf dir name IS the owner's agent id.
        let worktree = base.join("agents").join("sess-1").join("owner-id");
        std::fs::create_dir_all(&worktree).expect("mkdir worktree");

        let registry = WorktreeRegistry::new();
        registry.register(crate::worktree_create::WorktreeMeta {
            worktree: worktree.clone(),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "agent-owner-id".to_string(),
            name: "agent-owner-id".to_string(),
            agent_id: Some("owner-id".to_string()),
            session_id: "sess-1".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: "agent".to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        });

        // A NESTED agent (id != the worktree's owner dirname) stops with its cwd
        // resolved onto the owner's tree.
        dispose_worktree_in_background(
            &registry,
            "sess-1",
            Some("nested-guide-agent"),
            &worktree,
            false,
        );

        assert!(
            worktree.exists(),
            "the live owner's worktree dir is NOT deleted by a foreign stop",
        );
        assert_eq!(
            registry.get(&worktree).map(|m| m.worktree),
            Some(worktree.clone()),
            "the registration survives — the gate short-circuited before dispose",
        );
        assert!(
            registry.mark_surfaced(&worktree),
            "a foreign stop surfaces nothing — the path was never marked surfaced \
             (it never reached the dispose)",
        );
    }

    #[test]
    fn dispose_background_matching_owner_stop_passes_the_gate() {
        // Bug 103: the owner's OWN stop passes the identity gate and reaches the
        // `dispose` machinery (whose disposal outcomes are covered end-to-end in
        // `worktree_dispose`). Here the tree is a plain dir outside the scheme
        // root, so `dispose` returns `NotOurs` (no delete, no forget) — the point
        // is that the gate does NOT short-circuit a matching-owner stop, and the
        // registration is left for `dispose` to decide, not skipped by the gate.
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let worktree = base.join("agents").join("sess-1").join("owner-id");
        std::fs::create_dir_all(&worktree).expect("mkdir worktree");

        let registry = WorktreeRegistry::new();
        registry.register(crate::worktree_create::WorktreeMeta {
            worktree: worktree.clone(),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "agent-owner-id".to_string(),
            name: "agent-owner-id".to_string(),
            agent_id: Some("owner-id".to_string()),
            session_id: "sess-1".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: "agent".to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        });

        // The owner's own stop (id == the worktree's owner dirname) passes the gate.
        dispose_worktree_in_background(&registry, "sess-1", Some("owner-id"), &worktree, false);

        // The gate itself is the assertion of record.
        assert!(
            stop_owns_worktree("owner-id", &worktree),
            "the owner's own stop passes the dispose gate (dispose then decides)",
        );
        // Outside the scheme root, `dispose` classifies it `NotOurs`: the dir and
        // its registration are untouched (no forget, no surface) — the gate did
        // not swallow the call, `dispose`'s guard did.
        assert!(worktree.exists(), "a `NotOurs` path is never deleted");
        assert!(
            registry.mark_surfaced(&worktree),
            "a `NotOurs` disposition surfaces nothing — the path was never marked",
        );
    }

    #[test]
    fn dispose_background_host_initiated_removal_bypasses_the_identity_gate() {
        // The dirty arm / host path is unchanged (bug 103): a host-initiated
        // `WorktreeRemove` passes `None` for the stopping-agent id, so the identity
        // gate never fires — the human asked for this exact path. The tree here is
        // outside the scheme root, so `dispose` returns `NotOurs`; the point is
        // that `None` reaches `dispose` without an ownership check standing in the
        // way (a bare dir name that would FAIL the subagent gate).
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        // A `--worktree` (user-named) tree whose dirname is not an agent id — it
        // would never match a subagent gate, yet host removal must still proceed.
        let worktree = base.join("feats").join("repo").join("my-feature");
        std::fs::create_dir_all(&worktree).expect("mkdir worktree");

        let registry = WorktreeRegistry::new();
        registry.register(crate::worktree_create::WorktreeMeta {
            worktree: worktree.clone(),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "my-feature".to_string(),
            name: "my-feature".to_string(),
            agent_id: None,
            session_id: "sess-1".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: "feat".to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        });

        // Host-initiated dispose: `None` bypasses the gate entirely.
        dispose_worktree_in_background(&registry, "sess-1", None, &worktree, true);

        // `None` is not subject to ownership: had the gate applied it would have
        // skipped (dirname "my-feature" is no agent id). It reaches `dispose`,
        // which classifies the non-scheme path `NotOurs` (untouched).
        assert!(worktree.exists(), "a `NotOurs` path is never deleted");
        assert_eq!(
            registry.get(&worktree).map(|m| m.worktree),
            Some(worktree.clone()),
            "a `NotOurs` disposition leaves the registration",
        );
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
                pinned: Vec::new(),
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
    async fn subagent_stop_arms_countdown_and_keeps_mount() {
        // root-ownership 04: a SubagentStop reaches the daemon as
        // `post-agent/require-release`. The mount is PATH-keyed
        // (`worktree:{sid}:{canonical-path}`) — no identity — and SubagentStop no
        // longer tears it down: it ARMS the kept countdown so the servers stay
        // warm for a `land`/`rm`, with the countdown (reset by hook activity)
        // bounding an idle mount's lifetime. The mount persists across the stop.
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
        // Mount WITH an agent id — the contributor is nonetheless path-keyed.
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
            vec![format!("worktree:sess-1:{}", worktree.display())],
            "the mount is uniformly path-keyed, never identity-keyed",
        );

        // SubagentStop — arms the countdown by cwd, keeps the mount.
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
        // allowed to stop — empty response, never a block. Arming is a pure side
        // effect (decision 029).
        assert!(
            resp.trim().is_empty(),
            "require-release still allows the stop (arming is invisible): {resp:?}",
        );

        let roots = roots_ls(&ipc_path).await;
        assert!(
            roots.iter().any(|(p, _)| Path::new(p) == worktree),
            "the worktree MOUNT persists across SubagentStop (kept warm for land/rm; \
             the countdown, not the stop, retires it): {roots:?}",
        );
        // The seeded project root (other contributor) is untouched.
        assert!(
            roots.iter().any(|(p, _)| Path::new(p) == project),
            "the project root (hook contributor) survives",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kept_countdown_expiry_retires_mount_and_leaves_the_dirty_dir() {
        // The absolute (maintainer-verbatim, root-ownership 04): countdown expiry
        // retires the MOUNT ONLY (servers + root + lock) and PROVABLY leaves the
        // dirty directory in place. This drives the exact teardown the reaper runs
        // (`retire_root`) after arming the countdown, then asserts the worktree dir
        // still exists on disk.
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

        // Arm the countdown, then invoke the reaper's exact retire path.
        let ctx = manager.hook_ctx.as_ref().expect("hook_ctx").clone();
        let tracker = manager.root_tracker.as_ref().expect("tracker").clone();
        assert!(ctx.worktree_mounts.arm_countdown(&worktree, Instant::now()));
        retire_root(&ctx, &tracker, &worktree).await;

        // The MOUNT is gone (servers + root released)…
        assert!(
            !roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "countdown expiry retired the worktree mount",
        );
        // …the project root (a separate contributor) survives…
        assert!(
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == project),
            "the project root survives the worktree-mount retirement",
        );
        // …and the DIRECTORY is untouched — never auto-cleaned (the absolute).
        assert!(
            worktree.exists(),
            "expiry retires the mount ONLY; the dirty worktree directory persists \
             for land/rm (never auto-cleaned)",
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
    #[allow(
        clippy::too_many_lines,
        reason = "sequential mount/debt/stop/retry steps"
    )]
    async fn subagent_stop_outcome_gate_blocks_then_arms_countdown() {
        // Outcome gate (root-ownership 04): SubagentStop has two sequenced jobs.
        // While there is undelivered editing debt, require-release BLOCKS (the
        // agent is not stopping — it is about to run diagnostics in the worktree),
        // so the countdown must NOT arm and the mount is a LIVE (uncounted) mount.
        // The `stop_hook_active` retry then ALLOWS the stop and ARMS the kept
        // countdown — the mount persists, warm for a `land`/`rm`.
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

        // Give the subagent undelivered editing debt. The Stop-block is keyed to
        // the durable LEDGER now (bug 116), so the debt must be BOOKED there — a
        // real covered file under the worktree, acquired through the production
        // lock (`state_dir()/locks`). The in-memory batch records the same path as
        // the candidate set. Retired at the end so the production ledger is left
        // clean (this in-process test cannot isolate `state_dir()` via env —
        // Rust 2024 forbids `std::env::set_var`).
        let edited = worktree.join("src").join("main.rs");
        std::fs::create_dir_all(edited.parent().expect("parent")).expect("mk src");
        std::fs::write(&edited, b"fn main() {}\n").expect("write covered file");
        let edited = edited.canonicalize().expect("canon edited");
        let owner = crate::lock::Owner::new("test", "sess-1", "sub-1");
        let booking =
            crate::lock::Booking::from_config(&crate::config::Config::load().expect("cfg"));
        assert!(
            matches!(
                crate::lock::acquire(&edited, &owner, &booking, std::time::SystemTime::now()),
                crate::lock::Acquired::Ours
            ),
            "the covered edit books the worktree ledger"
        );
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
                edited.clone(),
                true,
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

        // The block left the mount a LIVE (uncounted) mount — no countdown armed.
        let key = format!("worktree:sess-1:{}", worktree.display());
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            assert!(
                ctx.worktree_mounts.kept_since(&key).is_none(),
                "a blocked stop does not arm the countdown (mount stays LIVE)",
            );
        }

        // Retry with `stop_hook_active` → the stop is ALLOWED → the countdown arms
        // and the mount PERSISTS (warm for land/rm; the countdown, not the stop,
        // retires it).
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
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "the allowed retry keeps the worktree mounted (countdown-governed)",
        );
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            assert!(
                ctx.worktree_mounts.kept_since(&key).is_some(),
                "the allowed retry armed the kept countdown on the mount",
            );
        }

        // Leave the production ledger clean — this in-process test booked into the
        // real `state_dir()/locks` (no in-process env isolation under Rust 2024).
        crate::lock::retire(&worktree);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_foreign_no_agent_id_arms_by_cwd() {
        // Foreign/legacy worktree: no `agent_id` at mount OR stop. The mount is
        // path-keyed (`worktree:{sid}:{path}`) and SubagentStop arms its countdown
        // via the cwd route (the enclosing worktree root of the stop cwd) — no
        // identity anywhere. The mount persists (countdown-governed).
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

        // SubagentStop WITHOUT an agent id → cwd route arms the path-keyed mount.
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
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "path-keyed foreign worktree mount persists at stop (countdown armed via cwd)",
        );
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let key = format!("worktree:sess-1:{}", worktree.display());
            assert!(
                ctx.worktree_mounts.kept_since(&key).is_some(),
                "the cwd route armed the kept countdown with no identity",
            );
        }

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_cwd_arms_from_subdirectory() {
        // The cwd route resolves the ENCLOSING worktree root, never an exact match
        // on the raw cwd: a final `cd` into a subdirectory of the worktree (the
        // host's carry-over default) still arms the enclosing mount's countdown.
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
            roots_ls(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == worktree),
            "the mount persists; enclosing-root resolution armed its countdown \
             even from a worktree subdirectory",
        );
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let key = format!("worktree:sess-1:{}", worktree.display());
            assert!(
                ctx.worktree_mounts.kept_since(&key).is_some(),
                "a subdirectory cwd resolves to the enclosing worktree and arms it",
            );
        }

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_stop_arms_the_cwd_worktree_not_a_foreign_one() {
        // Resolution is cwd-ONLY (root-ownership 04, AUDIT #10/#11): the stop arms
        // the countdown on the worktree its cwd resolves into (B), and never
        // reaches for an agent's other worktree (A) by identity. Both mounts
        // persist (arming keeps the mount); only B's countdown is armed.
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

        // Mount both worktrees (path-keyed uniformly).
        for wt in [&worktree_a, &worktree_b] {
            let _ = hook_roundtrip(
                &ipc_path,
                &serde_json::json!({
                    "method": "subagent-start/mount-worktree",
                    "session_id": "sess-1",
                    "agent_id": "sub-1",
                    "cwd": wt.display().to_string(),
                }),
            )
            .await;
        }

        // SubagentStop: agent id present but cwd is worktree B — cwd wins, arming B.
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
            roots.iter().any(|(p, _)| Path::new(p) == worktree_a),
            "worktree A stays mounted — the stop never reached for it: {roots:?}",
        );
        assert!(
            roots.iter().any(|(p, _)| Path::new(p) == worktree_b),
            "worktree B stays mounted (its countdown is armed): {roots:?}",
        );
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let key_a = format!("worktree:sess-1:{}", worktree_a.display());
            let key_b = format!("worktree:sess-1:{}", worktree_b.display());
            assert!(
                ctx.worktree_mounts.kept_since(&key_a).is_none(),
                "worktree A's countdown is NOT armed (cwd resolved to B)",
            );
            assert!(
                ctx.worktree_mounts.kept_since(&key_b).is_some(),
                "worktree B's countdown IS armed (the cwd worktree)",
            );
        }

        shutdown.cancel();
    }

    #[test]
    fn worktree_registry_rehydrates_from_sidecars() {
        // A fresh registry (what `with_session` builds on daemon start) rebuilds
        // the path→meta map by scanning the agents subtree for sidecars — a
        // daemon restart loses nothing durable (misc 150). The lookup is by
        // canonical path, not identity (root-ownership 04, AUDIT #10).
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
            registry.get(&wt).map(|m| m.worktree),
            Some(wt.clone()),
            "path→meta map rebuilt from the sidecar",
        );
        assert!(
            registry.get(&wt.with_file_name("other")).is_none(),
            "an unregistered path is absent",
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
    // bounded directory-deletion watch. These tests cover the teardown primitives
    // the watch reaper (`retire_root`), the GC, and the `SessionEnd` sweep all
    // share — dropping a contributor's root + watch `unregister` — and their
    // idempotence across those paths. The real-FS deletion→channel half (the OS
    // watch firing on a `remove_dir_all`) is covered by `worktree_watch::tests`'
    // `deletion_emits_contributor_event`; the full `retire_root` release edge is
    // covered by the integration test; here we drive the tracker + watcher
    // directly so the assertions stay deterministic (no OS-event timing).

    #[test]
    fn worktree_watch_reap_drops_only_the_deleted_root() {
        // The teardown primitive the reaper composes: dropping one watched
        // worktree's contributor and `unregister`ing its watch leaves every other
        // root in `global_roots` untouched. Mirrors
        // `reap_missing_worktree_roots_reaps_only_gone_dirs` but for the prompt
        // watch path rather than the hourly GC.
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
    fn heal_swapped_exe_trims_marker_when_target_lives() {
        // misc 182: after a rename-swap, /proc/self/exe reads `… (deleted)`
        // while the NEW binary sits at the original path — heal to the path.
        let dir = tempfile::tempdir().expect("tempdir");
        let living = dir.path().join("catenary");
        std::fs::write(&living, b"bin").expect("write");
        assert_eq!(
            heal_swapped_exe(dir.path().join("catenary (deleted)")),
            living
        );
    }

    #[test]
    fn heal_swapped_exe_keeps_marker_when_no_living_sibling() {
        // A binary genuinely named `… (deleted)` (or a swap that removed the
        // file without replacing it) passes through untouched.
        let dir = tempfile::tempdir().expect("tempdir");
        let deleted = dir.path().join("catenary (deleted)");
        assert_eq!(heal_swapped_exe(deleted.clone()), deleted);
    }

    #[test]
    fn heal_swapped_exe_passes_unmarked_path_through() {
        let p = PathBuf::from("/usr/bin/catenary");
        assert_eq!(heal_swapped_exe(p.clone()), p);
    }

    #[test]
    fn ephemeral_root_to_mount_detects_enclosing_and_skips_covered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, file) = marker_project(&base, "Lattice");
        let file = file.canonicalize().expect("canonicalize file");
        let deny = crate::answer_desk::SensitiveDenylist::load(&[]);

        // Outside every tracked root, enclosing `.git` detectable → mount it.
        let empty = HashSet::new();
        assert_eq!(
            ephemeral_root_to_mount(&file, &empty, &deny),
            EphemeralMountVerdict::Mount(project.clone()),
            "an out-of-root file mounts its enclosing project root",
        );

        // Already inside a tracked root → covered, no mount.
        let tracked: HashSet<PathBuf> = std::iter::once(project).collect();
        assert_eq!(
            ephemeral_root_to_mount(&file, &tracked, &deny),
            EphemeralMountVerdict::NoMount,
            "a file under a tracked root is already covered",
        );

        // No enclosing `.git` → no mount (the ticket-01 fallback still answers).
        let orphan = base.join("loose.txt");
        std::fs::write(&orphan, "x").expect("write");
        let orphan = orphan.canonicalize().expect("canon");
        assert_eq!(
            ephemeral_root_to_mount(&orphan, &empty, &deny),
            EphemeralMountVerdict::NoMount,
        );
    }

    #[test]
    fn ephemeral_mount_gate_refuses_sensitive_conversion_only() {
        // The sensitive-path gate (ws43-05) — one source of truth: the gate
        // consumes the ANSWER DESK's own compiled `SensitiveDenylist` (imported
        // from `crate::answer_desk`), so a path the desk flags is exactly a
        // path the gate refuses. No forked pattern logic exists to drift.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, plain) = marker_project(&base, "Lattice");
        let plain = plain.canonicalize().expect("canonicalize plain file");
        let secret = project.join("server.pem");
        std::fs::write(&secret, "hello secret\n").expect("write secret");
        let secret = secret.canonicalize().expect("canonicalize secret");

        let deny = crate::answer_desk::SensitiveDenylist::load(&[]);
        assert!(
            deny.is_sensitive(&secret),
            "the answer desk's denylist flags the fixture — the gate's premise",
        );

        // Sensitive + out-of-root + enclosing root detectable → the conversion
        // is REFUSED (never a mount), carrying the root it would have mounted.
        let empty = HashSet::new();
        assert_eq!(
            ephemeral_root_to_mount(&secret, &empty, &deny),
            EphemeralMountVerdict::RefusedSensitive(project.clone()),
            "a sensitive out-of-root path never converts into a mount",
        );

        // Non-sensitive path in the same project mounts exactly as today.
        assert_eq!(
            ephemeral_root_to_mount(&plain, &empty, &deny),
            EphemeralMountVerdict::Mount(project.clone()),
            "a non-sensitive out-of-root path still mounts",
        );

        // In-root sensitive path (root already tracked) → NoMount, not a
        // refusal: the gate governs mount conversion only.
        let tracked: HashSet<PathBuf> = std::iter::once(project.clone()).collect();
        assert_eq!(
            ephemeral_root_to_mount(&secret, &tracked, &deny),
            EphemeralMountVerdict::NoMount,
            "an in-root sensitive path changes nothing — no spurious refusal",
        );

        // A user `[permissions] deny_paths` extension gates through the same
        // desk-loaded list — the desk's semantics, not a parallel matcher.
        let user_deny = crate::answer_desk::SensitiveDenylist::load(&["**/vaulted/**".to_string()]);
        let vaulted_dir = project.join("vaulted");
        std::fs::create_dir_all(&vaulted_dir).expect("mkdir vaulted");
        let vaulted = vaulted_dir.join("plan.md");
        std::fs::write(&vaulted, "q3\n").expect("write vaulted");
        let vaulted = vaulted.canonicalize().expect("canonicalize vaulted");
        assert_eq!(
            ephemeral_root_to_mount(&vaulted, &empty, &user_deny),
            EphemeralMountVerdict::RefusedSensitive(project),
            "user deny_paths extensions refuse conversion too",
        );
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

    // ── Worktree-class roots: pinned-class lifetime + blocked display (misc 150 / bug 106) ──

    #[test]
    fn worktree_root_is_pinned_class_never_ephemeral() {
        // bug 106: a mounted worktree root carries a `worktree:*` contributor, so
        // it is NEVER classified ephemeral (`[ephemeral · expires when idle]`) — no
        // idle clock reaps it while its directory exists. The vanish-watch is its
        // only release edge. This pins the classification the CLI ls renders on.
        let root = PathBuf::from("/wt/pinned");
        let key = format!("worktree:sess:{}", root.display());
        assert!(
            !root_is_ephemeral(std::slice::from_ref(&key)),
            "a worktree-mounted root is pinned-class, never ephemeral",
        );
        assert!(
            !root_is_ephemeral(&[key, "ephemeral:/wt/pinned".to_string()]),
            "even alongside an ephemeral contributor, the worktree mount pins it",
        );
    }

    #[test]
    fn worktree_blocked_flag_marks_by_path_and_clears_on_activity() {
        // The blocked-on-permission flag is a pure display state (misc 151's
        // `catenary worktree ls` root-state). It is marked and cleared BY PATH
        // (the enclosing worktree of the PermissionRequest / activity cwd) — no
        // identity keying (root-ownership 04, AUDIT #11). The mount is never
        // expired by this — only its flag moves.
        let mounts = WorktreeMounts::new();
        let root = PathBuf::from("/wt/blocked");
        let key = format!("worktree:sess:{}", root.display());
        mounts.track(&key, &root);

        // A PermissionRequest resolving into the worktree marks it blocked.
        assert_eq!(
            mounts.mark_blocked_covering(&root.join("sub")),
            1,
            "the enclosing worktree is present and marked",
        );
        assert!(mounts.is_blocked(&key));

        // Qualifying activity resolving into the root clears the flag.
        mounts.touch_covering(&root.join("f.rs"), Instant::now());
        assert!(
            !mounts.is_blocked(&key),
            "activity under the root clears the blocked flag",
        );

        // The root's mount entry is never dropped by any of this — only teardown
        // (`remove`) or the kept countdown drops it.
        assert_eq!(
            mounts.mounted_roots(),
            vec![(root, false)],
            "the root stays mounted (unblocked) throughout",
        );
    }

    #[test]
    fn worktree_blocked_covering_marks_every_enclosing_root() {
        // A PermissionRequest whose cwd sits under two nested worktree roots marks
        // both; a sibling root is untouched. Resolved by PATH, no identity.
        let mounts = WorktreeMounts::new();
        let outer = PathBuf::from("/wt/outer");
        let inner = PathBuf::from("/wt/outer/inner");
        let other = PathBuf::from("/wt/other");
        mounts.track("worktree:sess-1:outer", &outer);
        mounts.track("worktree:sess-1:inner", &inner);
        mounts.track("worktree:sess-2:x", &other);

        // A prompt cwd under `inner` encloses both `inner` and `outer`.
        assert_eq!(mounts.mark_blocked_covering(&inner.join("f.rs")), 2);
        assert!(mounts.is_blocked("worktree:sess-1:outer"));
        assert!(mounts.is_blocked("worktree:sess-1:inner"));
        assert!(
            !mounts.is_blocked("worktree:sess-2:x"),
            "a sibling root outside the prompt's cwd is untouched",
        );
    }

    #[test]
    fn worktree_kept_countdown_expires_only_idle_armed_mounts() {
        // The kept countdown (root-ownership 04): only an ARMED mount idle past
        // `WORKTREE_KEPT_COUNTDOWN` is a reaper candidate. A LIVE mount (never
        // armed) is never expired; an armed mount refreshed by activity survives.
        let mounts = WorktreeMounts::new();
        let live = PathBuf::from("/wt/live");
        let idle = PathBuf::from("/wt/idle");
        let fresh = PathBuf::from("/wt/fresh");
        mounts.track("worktree:s:live", &live);
        mounts.track("worktree:s:idle", &idle);
        mounts.track("worktree:s:fresh", &fresh);

        let t0 = Instant::now();
        // Arm the two kept mounts at t0; `live` stays LIVE (never armed).
        assert!(mounts.arm_countdown(&idle, t0));
        assert!(mounts.arm_countdown(&fresh, t0));
        // A LIVE mount can never be armed by `arm_countdown` if absent; here
        // `live` simply is never armed, so `kept_since` stays None.
        assert!(mounts.kept_since("worktree:s:live").is_none());

        // Refresh `fresh` a moment after arming — its clock now trails `idle`'s by
        // a second, so a sweep exactly at `idle`'s deadline spares `fresh`.
        let refresh = t0 + Duration::from_secs(1);
        mounts.touch_covering(&fresh.join("f.rs"), refresh);

        // Sweep at exactly `idle`'s deadline (`t0 + COUNTDOWN`): `idle` lapsed
        // (elapsed == COUNTDOWN); `fresh` was refreshed 1s later so it trails by a
        // second and survives; `live` has no countdown.
        let now = t0 + WORKTREE_KEPT_COUNTDOWN;
        let expired = mounts.expired_countdowns(now, WORKTREE_KEPT_COUNTDOWN);
        assert_eq!(
            expired,
            vec![("worktree:s:idle".to_string(), idle)],
            "only the idle armed mount expires; live + refreshed survive: {expired:?}",
        );
    }

    #[test]
    fn worktree_kept_countdown_resets_on_covering_activity() {
        // Any hook resolving into the worktree resets the countdown (the one hook
        // seam). After arming and then touching, the last-activity moves forward,
        // so a sweep at the original deadline no longer expires it.
        let mounts = WorktreeMounts::new();
        let root = PathBuf::from("/wt/kept");
        mounts.track("worktree:s:kept", &root);

        let t0 = Instant::now();
        assert!(mounts.arm_countdown(&root, t0));

        // Activity at t0 + half the window refreshes the countdown to that instant.
        let mid = t0 + WORKTREE_KEPT_COUNTDOWN / 2;
        mounts.touch_covering(&root.join("src/main.rs"), mid);
        assert_eq!(mounts.kept_since("worktree:s:kept"), Some(mid));

        // At the ORIGINAL deadline the refreshed mount is not yet expired.
        let orig_deadline = t0 + WORKTREE_KEPT_COUNTDOWN;
        assert!(
            mounts
                .expired_countdowns(orig_deadline, WORKTREE_KEPT_COUNTDOWN)
                .is_empty(),
            "activity reset the countdown — not expired at the original deadline",
        );
        // A full window after the refresh, it expires.
        let after = mid + WORKTREE_KEPT_COUNTDOWN;
        assert_eq!(
            mounts
                .expired_countdowns(after, WORKTREE_KEPT_COUNTDOWN)
                .len(),
            1,
            "a full window after the last activity, the countdown expires",
        );
    }

    #[test]
    fn worktree_countdown_activity_lines_up_after_canonicalizing_the_seam() {
        // The spelling rule (d40a79b, extended to comparison seams): the countdown
        // reset compares an incoming cwd against the stored CANONICAL root by
        // `starts_with`. A symlinked-prefix alias of the root would NOT match the
        // canonical stored root lexically — so the one hook seam canonicalizes the
        // incoming cwd before `touch_covering`. This test pins that: the mount is
        // stored under a canonical root, and only the CANONICALIZED alias spelling
        // refreshes it (the raw alias, uncanonicalized, would silently miss).
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize base");
        // The real worktree root and an ALIAS symlink to it (an ancestor alias:
        // `<base>/alias` → `<base>/real`).
        let real = base.join("real");
        std::fs::create_dir_all(real.join("src")).expect("mkdir real/src");
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink alias → real");

        let mounts = WorktreeMounts::new();
        // The mount is stored under the CANONICAL root (as the daemon stores it).
        let canonical_root = real.canonicalize().expect("canonical real");
        mounts.track("worktree:s:real", &canonical_root);

        let t0 = Instant::now();
        assert!(mounts.arm_countdown(&canonical_root, t0));

        // A hook arrives with the ALIAS spelling of a file under the worktree.
        let aliased_cwd = alias.join("src");
        // The RAW alias spelling does not line up with the canonical stored root
        // (that is the whole hazard) — it refreshes nothing.
        let later = t0 + Duration::from_mins(1);
        mounts.touch_covering(&aliased_cwd, later);
        assert_eq!(
            mounts.kept_since("worktree:s:real"),
            Some(t0),
            "the RAW alias spelling misses the canonical root — no refresh (the hazard)",
        );

        // The seam's fix: canonicalize the incoming cwd first (what the one hook
        // seam does). The canonicalized alias now lines up and refreshes.
        let canonical_cwd = aliased_cwd.canonicalize().expect("canonical aliased cwd");
        mounts.touch_covering(&canonical_cwd, later);
        assert_eq!(
            mounts.kept_since("worktree:s:real"),
            Some(later),
            "canonicalizing the incoming cwd at the seam lines it up with the \
             canonical stored root — the countdown resets",
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

        // A grep hit-batch touching the out-of-root file mounts its enclosing
        // project root (the annotator's per-batch auto-mount, ws43-02).
        let _ = hitstream_annotate_one(&ipc_path, &file, "hello").await;

        let classes = roots_ls_classes(&ipc_path).await;
        let entry = classes
            .iter()
            .find(|(p, _)| Path::new(p) == project)
            .expect("enclosing project mounted ephemerally on out-of-root grep");
        assert!(entry.1, "the activity-mounted root is classed ephemeral");

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grep_out_of_root_sensitive_path_streams_hit_but_never_mounts() {
        // The sensitive-path gate on query auto-mount (ws43-05): a query
        // touching a sensitive out-of-root path still returns its hit
        // (decision 025 — complete output), but the enclosing root NEVER
        // converts into a mount — no tracker entry, hence no server spawn.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, _) = marker_project(&base, "Lattice");
        // `server.pem` matches the shipped `**/*.pem` denylist entry.
        let secret = project.join("server.pem");
        std::fs::write(&secret, "hello secret\n").expect("write secret");

        // No mounted roots — the file is outside every root.
        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        let resp = hitstream_annotate_one(&ipc_path, &secret, "hello secret").await;
        // The hit still streams — the gate is on mount state, never on results:
        // the annotation-batch echoes the hit (unenriched) rather than dropping it.
        assert!(
            resp.contains("hello secret"),
            "the sensitive hit is still returned (unenriched): {resp}",
        );

        // But nothing mounted: the enclosing project never appears on the board.
        let classes = roots_ls_classes(&ipc_path).await;
        assert!(
            classes.iter().all(|(p, _)| Path::new(p) != project),
            "a sensitive touched path never converts into a mount: {classes:?}",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ephemeral_root_under_hook_activity_does_not_idle_expire() {
        // Acceptance (root-ownership 04): an ephemeral root under active hook
        // traffic (edits/reads, no queries) does not idle-expire. Every hook
        // carries cwd, and the one hook seam refreshes the covering ephemeral
        // root's idle clock — so a stream of ordinary hooks (a Read PreToolUse
        // whose cwd is inside the ephemeral root) keeps it alive with NO query.
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

        // Mount the ephemeral root via an out-of-root grep hit-batch (the one
        // query allowed — mounting is a query's job; refreshing is the hook
        // seam's).
        let _ = hitstream_annotate_one(&ipc_path, &file, "hello").await;

        // Age the ephemeral clock deep into the idle window by hand (no wall-clock
        // wait): set its last-activity to a stale instant, so absent a refresh the
        // next sweep would reap it. `checked_sub` keeps the arithmetic panic-free.
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let stale = Instant::now()
                .checked_sub(EPHEMERAL_ROOT_IDLE_TIMEOUT)
                .expect("a monotonic instant far enough in the past");
            ctx.ephemeral_mounts.touch(&project, stale);
        }

        // A NON-query hook — a Read PreToolUse whose cwd sits inside the ephemeral
        // root — flows through the one hook seam and refreshes the clock.
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "pre-tool/editing-state",
                "tool_name": "Read",
                "session_id": "sess-1",
                "cwd": project.display().to_string(),
                "host_payload": { "cwd": project.display().to_string() },
            }),
        )
        .await;

        // The clock is refreshed: its remaining is back near the full timeout —
        // well above the stale (fully-elapsed) value it held before the hook.
        {
            let ctx = manager.hook_ctx.as_ref().expect("hook_ctx");
            let remaining = ctx
                .ephemeral_mounts
                .idle_remaining(&project, Instant::now(), EPHEMERAL_ROOT_IDLE_TIMEOUT)
                .expect("the ephemeral root still carries an idle clock");
            assert!(
                remaining > EPHEMERAL_ROOT_IDLE_TIMEOUT / 2,
                "hook activity reset the ephemeral clock near full ({remaining:?}); \
                 the root does not idle-expire under active hook traffic",
            );
        }
        // And it is still mounted — no idle expiry happened.
        assert!(
            roots_ls_classes(&ipc_path)
                .await
                .iter()
                .any(|(p, eph)| Path::new(p) == project && *eph),
            "the ephemeral root survives under hook activity",
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_edit_preheats_the_cold_root_before_any_diagnose() {
        // Acceptance (root-ownership stage 6, deliverable 3 — the bug-108
        // absorption): the FIRST edit in a cold root — no prior grep/glob warmed
        // it — fires the ensure path so the enclosing project root mounts (its
        // server starts) BEFORE the first `catenary diagnostics`. Observed via the
        // daemon's root board (`tool/roots-ls`): the cold root is mounted right
        // after the edit hook, with no query in between.
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let (project, file) = marker_project(&base, "Cold");

        let manager = Arc::new(bind_with_session(dir.path()));
        let shutdown = manager.shutdown_token();
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = m.accept_loop().await;
        });

        // The root is cold: no query has mounted it.
        assert!(
            !roots_ls_classes(&ipc_path)
                .await
                .iter()
                .any(|(p, _)| Path::new(p) == project),
            "the root is cold before the first edit — no prior query warmed it",
        );

        // A single Edit PreToolUse for a file in the cold root. The hook cwd sits
        // OUTSIDE the project (so only the edited file_path can drive the mount —
        // proving the preheat rides the edit path, not the cwd seam).
        let _ = hook_roundtrip(
            &ipc_path,
            &serde_json::json!({
                "method": "pre-tool/editing-state",
                "tool_name": "Edit",
                "session_id": "sess-1",
                "file_path": file.display().to_string(),
                "cwd": base.display().to_string(),
                "host_payload": { "cwd": base.display().to_string() },
            }),
        )
        .await;

        // The edit preheated the cold root: it is now mounted (ephemeral),
        // BEFORE any diagnose ran.
        let entry = roots_ls_classes(&ipc_path)
            .await
            .into_iter()
            .find(|(p, _)| Path::new(p) == project);
        let (_, ephemeral) = entry.expect("the first edit mounted the enclosing cold root");
        assert!(
            ephemeral,
            "the preheat mount is an ephemeral activity mount",
        );

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

        // Mount ephemerally via an out-of-root grep hit-batch.
        let _ = hitstream_annotate_one(&ipc_path, &file, "hello").await;
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

    /// Bug 109 seal: an in-process `tool/roots-add` persists to the INJECTED
    /// tempdir config, never the operator's real `~/.config/catenary/config.toml`.
    ///
    /// Before the fix, `persist_pin` re-resolved [`user_config_path`] through
    /// [`crate::paths::config_dir`], which no in-process test can redirect (Rust
    /// 2024 forbids `std::env::set_var`), so every such pin wrote the maintainer's
    /// real config — the eight-month poison. Here the config path is injected by
    /// `bind_with_session_config` into this test's tempdir; we assert the pin
    /// landed there. The whole in-process router suite is protected by that same
    /// injection; this test names the guarantee and would go red if the injection
    /// ever regressed (the pin would silently vanish from the tempdir).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_process_pin_persists_to_injected_config_not_the_real_one() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ipc_path = ipc_socket_in(dir.path());
        let base = dir.path().canonicalize().expect("canonicalize");
        let project = base.join("project");
        std::fs::create_dir_all(&project).expect("mkdir project");

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

        // The injected config `bind_with_session_config` wired lives under this
        // test's own tempdir — never `~/.config`.
        let injected = dir
            .path()
            .join("config")
            .join("catenary")
            .join("config.toml");
        assert!(
            injected.exists(),
            "the pin persisted to the injected tempdir config at {}",
            injected.display(),
        );
        let text = std::fs::read_to_string(&injected).expect("read injected config");
        let doc: toml::Value = toml::from_str(&text).expect("valid toml");
        let pinned = doc["roots"]["pinned"]
            .as_array()
            .expect("pinned array in the injected config");
        // The entry is written home-compressed (`~/…` under `$HOME`), so expand
        // the tilde back before comparing with the canonical project path.
        assert!(
            pinned
                .iter()
                .filter_map(toml::Value::as_str)
                .any(|entry| { Path::new(&crate::bridge::expand_tilde(entry)) == project }),
            "the injected config carries the pin, proving the real user config was \
             never the write target: {text}",
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

    /// IPC method constants match expected wire values. `tool/glob` retired
    /// with the ws43-03 cutover — the hitstream arm is the only search method.
    #[test]
    fn method_constants() {
        assert_eq!(METHOD_HITSTREAM, "tool/hitstream");
        // The router constant re-exports the protocol module's owner (ws43), so
        // the two spellings can never drift.
        assert_eq!(METHOD_HITSTREAM, crate::hitstream::HITSTREAM_METHOD);
    }

    // ── Bridge tenacity: the indefinite connect loop (pulse 02) ────────

    use crate::daemon_intent::Intent;
    use std::cell::{Cell, RefCell};

    /// No marker, daemon unreachable: the loop retries far past both retired
    /// give-up budgets (the startup 10-attempt budget and the reconnect
    /// 30-round budget, ~300 ticks combined) without erroring, respawn
    /// allowed, and connects the moment a socket appears.
    #[test]
    fn tenacity_outlives_the_retired_budgets_and_respawns() {
        let connects = Cell::new(0u32);
        let spawns = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || {
                connects.set(connects.get() + 1);
                (connects.get() > 350).then_some(())
            },
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || true,
            || None,
            |_| {},
        );

        assert_eq!(outcome, TenaciousOutcome::Connected(()));
        assert_eq!(
            connects.get(),
            351,
            "every tick tries the socket — no budget cuts the loop short",
        );
        assert!(spawns.get() >= 1, "the no-marker crash path may spawn");
    }

    /// The backoff starts at the floor (100 ms), doubles each tick, and caps
    /// at 5 s.
    #[test]
    fn tenacity_backoff_doubles_to_cap() {
        let connects = Cell::new(0u32);
        let sleeps = RefCell::new(Vec::new());

        let outcome = connect_with_tenacity(
            || {
                connects.set(connects.get() + 1);
                (connects.get() > 10).then_some(())
            },
            || Ok(()),
            || true,
            || None,
            |d| sleeps.borrow_mut().push(d),
        );

        assert_eq!(outcome, TenaciousOutcome::Connected(()));
        let millis: Vec<u128> = sleeps
            .borrow()
            .iter()
            .map(std::time::Duration::as_millis)
            .collect();
        assert_eq!(
            millis,
            vec![100, 200, 400, 800, 1600, 3200, 5000, 5000, 5000, 5000],
            "capped exponential backoff: 100 ms doubling to a 5 s cap",
        );
    }

    /// Marker `stop`: connect-only — the loop never spawns, but reattaches as
    /// soon as a socket appears.
    #[test]
    fn tenacity_stop_marker_never_spawns_and_reattaches() {
        let connects = Cell::new(0u32);
        let spawns = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || {
                connects.set(connects.get() + 1);
                (connects.get() > 50).then_some(())
            },
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || true,
            || Some(Intent::Stop),
            |_| {},
        );

        assert_eq!(
            outcome,
            TenaciousOutcome::Connected(()),
            "a socket appearing under `stop` is reattached to",
        );
        assert_eq!(spawns.get(), 0, "`stop` means never spawn");
    }

    /// A `stop` marker cleared mid-wait (a `catenary start` happened) is
    /// picked up on the next tick: the loop may spawn again.
    #[test]
    fn tenacity_cleared_stop_marker_reenables_spawn() {
        let connects = Cell::new(0u32);
        let spawns = Cell::new(0u32);
        let intent_reads = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || {
                connects.set(connects.get() + 1);
                (connects.get() > 10).then_some(())
            },
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || true,
            || {
                intent_reads.set(intent_reads.get() + 1);
                (intent_reads.get() <= 5).then_some(Intent::Stop)
            },
            |_| {},
        );

        assert_eq!(outcome, TenaciousOutcome::Connected(()));
        assert_eq!(
            spawns.get(),
            1,
            "no spawn while stopped; exactly one after the marker cleared",
        );
    }

    /// Marker `quit` at socket-loss/spawn-time: prompt exit — consulted before
    /// the connect attempt, so nothing is tried and nothing is spawned.
    #[test]
    fn tenacity_quit_marker_exits_promptly() {
        let connects = Cell::new(0u32);
        let spawns = Cell::new(0u32);
        let sleeps = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || {
                connects.set(connects.get() + 1);
                Some(())
            },
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || true,
            || Some(Intent::Quit),
            |_| sleeps.set(sleeps.get() + 1),
        );

        assert_eq!(outcome, TenaciousOutcome::QuitRequested);
        assert_eq!(connects.get(), 0, "quit wins before the connect attempt");
        assert_eq!(spawns.get(), 0, "quit never spawns");
        assert_eq!(sleeps.get(), 0, "quit never waits");
    }

    /// A `quit` marker appearing mid-wait takes effect on the next tick.
    #[test]
    fn tenacity_quit_marker_mid_wait_takes_next_tick() {
        let intent_reads = Cell::new(0u32);
        let sleeps = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || None::<()>,
            || Ok(()),
            || true,
            || {
                intent_reads.set(intent_reads.get() + 1);
                (intent_reads.get() > 3).then_some(Intent::Quit)
            },
            |_| sleeps.set(sleeps.get() + 1),
        );

        assert_eq!(outcome, TenaciousOutcome::QuitRequested);
        assert_eq!(
            sleeps.get(),
            3,
            "three no-marker waiting ticks, then the quit is obeyed",
        );
    }

    /// Stdin EOF is the only unconditional self-exit: the loop ends cleanly
    /// the tick it observes the closed stdin, however long it has waited.
    #[test]
    fn tenacity_stdin_eof_exits_clean() {
        let stdin_polls = Cell::new(0u32);
        let sleeps = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || None::<()>,
            || Ok(()),
            || {
                stdin_polls.set(stdin_polls.get() + 1);
                stdin_polls.get() <= 5
            },
            || None,
            |_| sleeps.set(sleeps.get() + 1),
        );

        assert_eq!(outcome, TenaciousOutcome::StdinClosed);
        assert_eq!(sleeps.get(), 5, "the loop exits the tick stdin closes");
    }

    /// Respawns are paced by [`SPAWN_RETRY_INTERVAL`] of accumulated waiting:
    /// a slow-binding daemon is not doubled up on each tick, but a spawn that
    /// died before binding IS retried once the grace lapses.
    #[test]
    fn tenacity_respawn_paced_by_grace_interval() {
        let connects = Cell::new(0u32);
        let spawns = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || {
                connects.set(connects.get() + 1);
                // Backoff accumulates 31.3 s over ticks 0-10, so the grace
                // (30 s) lapses exactly once before the tick-12 connect.
                (connects.get() > 12).then_some(())
            },
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || true,
            || None,
            |_| {},
        );

        assert_eq!(outcome, TenaciousOutcome::Connected(()));
        assert_eq!(
            spawns.get(),
            2,
            "one spawn up front, one respawn after the grace lapsed",
        );
    }

    /// A failed spawn attempt (nothing launched) does not arm the pacing —
    /// the very next tick retries it.
    #[test]
    fn tenacity_failed_spawn_retries_next_tick() {
        let connects = Cell::new(0u32);
        let spawns = Cell::new(0u32);

        let outcome = connect_with_tenacity(
            || {
                connects.set(connects.get() + 1);
                (connects.get() > 3).then_some(())
            },
            || {
                spawns.set(spawns.get() + 1);
                anyhow::bail!("exe missing mid-swap")
            },
            || true,
            || None,
            |_| {},
        );

        assert_eq!(outcome, TenaciousOutcome::Connected(()));
        assert_eq!(
            spawns.get(),
            3,
            "a spawn that launched nothing is retried every tick",
        );
    }
}
