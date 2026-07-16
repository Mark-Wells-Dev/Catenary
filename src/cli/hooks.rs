// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Hook handlers for host CLI integration.
//!
//! Each function is a thin transport: read stdin from the host CLI,
//! connect to the daemon's hook socket, forward the request as a
//! [`HookRequest`](crate::hook::HookRequest), and format the response
//! for the host. The `session_id` is sent in the payload — routing
//! to the correct session happens daemon-side.
//!
//! All hook logic runs server-side in `HookServer` (`src/hook.rs`).
//!
//! Function names mirror the hook lifecycle:
//! - `run_pre_tool` — editing state enforcement (`PreToolUse` / `BeforeTool`)
//! - `run_post_agent` — force `done_editing` (`Stop` / `AfterAgent`)
//! - `run_session_start` — clear stale editing state (`SessionStart`)
//! - `run_pre_invocation` — first-sighting teaching injection (Antigravity
//!   `PreInvocation`)
//! - `run_reserved_shim` — every event registered ahead of its behavior
//!   (full-surface registration, pre-v2): drain stdin, exit 0

#![allow(
    clippy::print_stdout,
    reason = "hook output is stdout JSON for the host CLI"
)]

use std::path::PathBuf;
use std::time::Duration;

use crate::cli::HostFormat;
use crate::cli::command_filter::resolver::LineWrites;

// ── Teaching payload injection (ws36 ticket 01) ──────────────────────────

/// Decide whether a `SessionStart` hook should inject the teaching payload,
/// given the payload's `source` field.
///
/// The payload is injected whenever the new context does *not* already contain
/// it:
/// - `startup` (fresh), `clear` (`/clear` wiped it), `compact` (summarization
///   may have dropped it) → **inject**.
/// - `resume` → **skip** — resume restores the prior transcript verbatim, so the
///   payload is already present.
///
/// A missing or unknown `source` is treated as **inject**: a missing source most
/// likely means a fresh start, and only `resume` is a context that provably
/// already carries the payload.
#[must_use]
fn session_start_should_announce(source: Option<&str>) -> bool {
    source != Some("resume")
}

/// The teaching payload to inject at `SessionStart`, or `None` when it should
/// be withheld (a non-inject `source`, or a host whose `SessionStart` cannot
/// carry it). Claude Code reads the `hookSpecificOutput.additionalContext` field
/// at `SessionStart`; Antigravity and OpenCode use other channels. Computed only
/// when it will be used, so the config-load IO is skipped on the withhold paths.
///
/// The `--format` flag on the hook definition is the declared client identity
/// (misc 177), so the payload is keyed by it — Claude's hook set carries
/// `WorktreeCreate`, so its payload teaches worktree-isolated dispatch.
#[must_use]
fn session_start_context(announce: bool, format: HostFormat) -> Option<String> {
    (announce && matches!(format, HostFormat::Claude))
        .then(|| crate::cli::teaching::emitted_payload(Some(format)))
}

/// Append the cross-session lingering-worktree line (misc 151 D-2) to a
/// `SessionStart` context, when there is one and orphans exist.
///
/// Only augments a `Some` context (the announce+Claude path), so the pure
/// [`session_start_context`] stays hermetic and this filesystem scan runs only in
/// the live hook.
fn with_orphan_line(ctx: Option<String>) -> Option<String> {
    let base = ctx?;
    match cross_session_orphan_line(&crate::paths::agents_worktrees_dir()) {
        Some(line) => Some(format!("{base}\n\n{line}")),
        None => Some(base),
    }
}

/// Append the bridge↔daemon protocol-version mismatch line (ws41-02) to a
/// `SessionStart` context, when there is one and the daemon has recorded a
/// mismatch.
///
/// Only augments a `Some` context (the announce+Claude path), so
/// [`session_start_context`] stays hermetic and this snapshot read runs only in
/// the live hook. The line names the older side and its cure — the persistent
/// reminder that carries beneath the daemon's one-time desktop interrupt until
/// the versions agree.
fn with_bridge_mismatch_line(ctx: Option<String>) -> Option<String> {
    let base = ctx?;
    match bridge_mismatch_line() {
        Some(line) => Some(format!("{base}\n\n{line}")),
        None => Some(base),
    }
}

/// The one-line bridge↔daemon version-mismatch note, or `None` when no daemon is
/// up, the versions agree, or the daemon predates the recorded field.
///
/// Reads the mismatch the daemon recorded onto its snapshot and renders the
/// shared direction-aware wording ([`catenary_mcp::version_mismatch`]), so the
/// `SessionStart` line, the `catenary doctor` finding, and the TUI board finding
/// all say the same thing ("bridge is `X`, daemon links `Y` — run `/mcp`").
#[must_use]
fn bridge_mismatch_line() -> Option<String> {
    let recorded = crate::state_snapshot::Snapshot::read_default()?
        .daemon
        .bridge_mismatch?;
    let mismatch = catenary_mcp::version_mismatch(
        recorded.bridge_version.as_deref(),
        &recorded.daemon_version,
    )?;
    Some(mismatch.message())
}

/// A one-line mention of agent worktrees lingering from previous sessions (misc
/// 151 D-2 cross-session orphans), or `None` when there are none.
///
/// Scans `agents_root` for sidecars whose worktree dir still exists — a present
/// agent worktree at session start belongs to a prior (now-dead) session. These
/// get this passive mention plus the `catenary worktree ls` pointer, not a block
/// (the nag is a same-session doorbell).
#[must_use]
fn cross_session_orphan_line(agents_root: &std::path::Path) -> Option<String> {
    let count = crate::worktree_create::scan_sidecars(agents_root)
        .into_iter()
        .filter(|meta| meta.worktree.exists())
        .count();
    if count == 0 {
        return None;
    }
    let verb = if count == 1 {
        "worktree lingers"
    } else {
        "worktrees linger"
    };
    Some(format!(
        "{count} agent {verb} from previous sessions — run `catenary worktree ls`."
    ))
}

/// The raw stdout body emitted by `catenary hook session-start
/// --format=opencode`.
///
/// The live teaching payload verbatim — the same SSOT rendering as `catenary
/// primer` and the Claude `SessionStart` `additionalContext`, keyed by the
/// declared OpenCode identity (whose hook set carries no `WorktreeCreate`, so
/// it matches the client-neutral payload) — for the OpenCode plugin to write
/// into its runtime-regenerated instructions file. Not a JSON envelope: the
/// plugin captures stdout as the file's content, so this is the bare payload
/// text (unlike the Claude/Antigravity structured responses).
#[must_use]
fn opencode_session_start_body() -> String {
    crate::cli::teaching::emitted_payload(Some(HostFormat::OpenCode))
}

/// Build the Antigravity `PreInvocation` output that injects `payload` as a
/// **persisted** `injectSteps` `userMessage` (teaching-surface ticket 03; the
/// payload is the per-session sliver as of ticket 14).
///
/// `payload` is the per-session teaching sliver
/// ([`crate::cli::teaching::session_sliver`]) — the cwd build tool the always-on
/// rules file structurally cannot carry — not the full teaching body (that rides
/// the rules file every turn). A `userMessage` step is written into the
/// conversation transcript and stales like any transcript content, unlike the
/// per-model-call `ephemeralMessage` channel (excluded by maintainer ruling — it
/// is transient per call, not a session-start surface). The daemon-side
/// first-sighting ledger gates emission to exactly once per conversation, so this
/// rides no per-turn cadence.
#[must_use]
fn pre_invocation_injection(payload: &str) -> String {
    serde_json::json!({
        "injectSteps": [ { "userMessage": payload } ],
    })
    .to_string()
}

/// The Antigravity `PreInvocation` no-op output — a bare JSON object that
/// injects nothing.
///
/// Emitted on every model call that is *not* a conversation's first sighting
/// (the common case), and on the fail-closed paths (unparsable stdin, a
/// non-Antigravity host, or an unreachable daemon). Matches the invocation-hook
/// contract's "nothing to do" shape (`{}`), so injection happens once and only
/// once.
#[must_use]
fn empty_pre_invocation() -> String {
    "{}".to_string()
}

/// Build the `hookSpecificOutput` object that carries the teaching payload in
/// its `additionalContext` field, for the given hook event.
///
/// `hook_event_name` is `"SessionStart"` or `"SubagentStart"` — the Claude Code
/// structured-output shape is
/// `{"hookSpecificOutput": {"hookEventName": <event>, "additionalContext": <string>}}`.
/// This builds only the inner object; callers nest it under `hookSpecificOutput`.
#[must_use]
fn announcement_hook_specific_output(
    hook_event_name: &str,
    additional_context: &str,
) -> serde_json::Value {
    serde_json::json!({
        "hookEventName": hook_event_name,
        "additionalContext": additional_context,
    })
}

/// Connect to the daemon's IPC endpoint.
///
/// When the socket file exists but the connection fails (daemon likely
/// crashed), emits an `error!()` event — but only on the *first* hook to
/// witness a given stranded socket. The hook CLI is one short-lived process
/// per tool call, so an in-process debounce cannot span invocations; the
/// cross-process [`UnreachableStamp`](crate::notify::UnreachableStamp) keyed to
/// the socket's filesystem identity is what keeps one strand to one interrupt
/// (bug 111 — the 26-notification storm). A NEW strand (a different socket
/// inode) or a successful daemon bind re-arms the stamp. The hook CLI's tracing
/// subscriber routes the `error!()` to a desktop notification so the user gets
/// an immediate signal outside the agent's context.
#[cfg(unix)]
fn hook_connect(_hook_json: &serde_json::Value) -> Option<std::os::unix::net::UnixStream> {
    let daemon_path = crate::router::socket_path();
    let result = notify_connect(&daemon_path);
    if result.is_none()
        && daemon_path.exists()
        && crate::notify::UnreachableStamp::new().should_notify(&daemon_path)
    {
        // Socket exists but connection failed — daemon likely crashed. First
        // hook to see this strand fires the one interrupt it earns; later hooks
        // find a matching stamp and stay silent.
        tracing::error!(
            source = crate::source::Source::HookDispatch.as_str(),
            "Catenary daemon unreachable (socket exists but connection failed). Run: catenary doctor",
        );
    }
    result
}

/// Connect to the daemon's hook IPC endpoint (Windows stub).
///
/// The daemon is Unix-only. Windows hook support is planned.
#[cfg(windows)]
fn hook_connect(_hook_json: &serde_json::Value) -> Option<std::fs::File> {
    None
}

/// Connects to a notify IPC endpoint and returns a stream for I/O.
///
/// Returns `None` silently on failure (hooks must not break Claude Code's flow).
#[cfg(unix)]
fn notify_connect(endpoint: &std::path::Path) -> Option<std::os::unix::net::UnixStream> {
    if !endpoint.exists() {
        return None;
    }
    let stream = std::os::unix::net::UnixStream::connect(endpoint).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_mins(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    Some(stream)
}

/// Connects to a notify IPC endpoint and returns a stream for I/O.
///
/// Returns `None` silently on failure (hooks must not break Claude Code's flow).
#[cfg(windows)]
fn notify_connect(endpoint: &std::path::Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // SECURITY_IDENTIFICATION (0x0001_0000) prevents impersonation attacks
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(0x0001_0000)
        .open(endpoint)
        .ok()
}

/// Sends a JSON request over an IPC stream and reads response lines.
fn ipc_exchange(
    mut stream: impl std::io::Read + std::io::Write,
    request: &serde_json::Value,
) -> Vec<String> {
    use std::io::BufRead;

    if serde_json::to_writer(&mut stream, request).is_err() {
        return Vec::new();
    }
    if stream.write_all(b"\n").is_err() || stream.flush().is_err() {
        return Vec::new();
    }

    let reader = std::io::BufReader::new(stream);
    let mut lines = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(text) if !text.is_empty() => lines.push(text),
            _ => break,
        }
    }
    lines
}

/// Format a `PreToolUse` deny response for the host CLI.
fn format_deny(reason: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        })
        .to_string(),
        // Antigravity uses `{decision: "deny", reason}`; the OpenCode plugin
        // consumes the same shape directly (`catenary.js`), surfacing `reason` as
        // the thrown block message.
        HostFormat::Antigravity | HostFormat::OpenCode => serde_json::json!({
            "decision": "deny",
            "reason": reason
        })
        .to_string(),
    }
}

/// Format a Stop/AfterAgent block response for the host CLI.
fn format_stop_block(reason: &str, format: HostFormat) -> String {
    match format {
        // OpenCode registers only `tool.execute.before` (no Stop/AfterAgent
        // surface), so this is never emitted for OpenCode; it shares Claude's
        // `{decision: "block", reason}` shape as a safe never-reached default.
        HostFormat::Claude | HostFormat::OpenCode => serde_json::json!({
            "decision": "block",
            "reason": reason
        })
        .to_string(),
        HostFormat::Antigravity => serde_json::json!({
            "decision": "continue",
            "reason": reason
        })
        .to_string(),
    }
}

/// Extract session ID from hook payload.
///
/// Each host uses a different field name for the session identifier.
fn extract_session_id(hook_json: &serde_json::Value, format: HostFormat) -> Option<&str> {
    match format {
        HostFormat::Claude => hook_json.get("session_id"),
        HostFormat::Antigravity => hook_json.get("conversationId"),
        // OpenCode plugin payload carries `sessionID` (`catenary.js`).
        HostFormat::OpenCode => hook_json.get("sessionID"),
    }
    .and_then(|v| v.as_str())
}

/// Extract working directory from hook payload.
///
/// Each host uses a different field name and shape for the working directory.
fn extract_cwd_str(hook_json: &serde_json::Value, format: HostFormat) -> Option<&str> {
    match format {
        HostFormat::Claude => hook_json.get("cwd").and_then(|v| v.as_str()),
        HostFormat::Antigravity => hook_json
            .get("workspacePaths")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str()),
        // OpenCode plugin payload carries `directory` (`catenary.js`).
        HostFormat::OpenCode => hook_json.get("directory").and_then(|v| v.as_str()),
    }
}

/// Extract tool name from hook payload.
///
/// Each host uses a different field path for the tool name.
fn extract_tool_name(hook_json: &serde_json::Value, format: HostFormat) -> &str {
    match format {
        HostFormat::Claude => hook_json.get("tool_name").and_then(|v| v.as_str()),
        HostFormat::Antigravity => hook_json
            .get("toolCall")
            .and_then(|tc| tc.get("name"))
            .and_then(|v| v.as_str()),
        // OpenCode plugin forwards `input.tool` as the top-level `tool` field
        // (`catenary.js`) — OpenCode's own lowercase tool name (`read`/`edit`/
        // `write`/`bash`/…).
        HostFormat::OpenCode => hook_json.get("tool").and_then(|v| v.as_str()),
    }
    .unwrap_or("")
}

/// Extract `agent_id` from hook payload. Defaults to empty string (main agent).
fn extract_agent_id(hook_json: &serde_json::Value) -> &str {
    hook_json
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// Extracts the file path from hook JSON.
///
/// Each host uses a different field path for the edited file.
fn extract_file_path(hook_json: &serde_json::Value, format: HostFormat) -> Option<String> {
    let file_path = match format {
        HostFormat::Claude => hook_json
            .get("tool_input")
            .and_then(|ti| ti.get("file_path").or_else(|| ti.get("file")))
            .and_then(|fp| fp.as_str()),
        HostFormat::Antigravity => hook_json
            .get("toolCall")
            .and_then(|tc| tc.get("args"))
            .and_then(|a| a.get("TargetFile"))
            .and_then(|fp| fp.as_str()),
        // OpenCode forwards `output.args` under the `args` field (`catenary.js`);
        // its `read`/`edit`/`write` tools name the target `filePath`.
        HostFormat::OpenCode => hook_json
            .get("args")
            .and_then(|a| a.get("filePath"))
            .and_then(|fp| fp.as_str()),
    }?;

    // Resolve to absolute path
    let abs_path = if std::path::Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        let cwd = extract_cwd_str(hook_json, format).map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );
        cwd.join(file_path)
    };

    Some(abs_path.to_string_lossy().into_owned())
}

/// Extracts the worktree path from a `WorktreeRemove` hook payload.
///
/// The Claude Code `WorktreeRemove` payload carries the absolute path of the
/// worktree being removed under `worktree_path`. Resolved to an absolute path
/// against `cwd` (mirroring [`extract_file_path`]) for the rare case the host
/// sends a relative path; the daemon canonicalizes it to agree with the mount
/// key by construction.
fn extract_worktree_path(hook_json: &serde_json::Value, format: HostFormat) -> Option<String> {
    let worktree_path = hook_json.get("worktree_path").and_then(|v| v.as_str())?;

    let abs_path = if std::path::Path::new(worktree_path).is_absolute() {
        PathBuf::from(worktree_path)
    } else {
        let cwd = extract_cwd_str(hook_json, format).map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );
        cwd.join(worktree_path)
    };

    Some(abs_path.to_string_lossy().into_owned())
}

// ── Host payload capture ───────────────────────────────────────────────

/// Maximum character count for string values in the host payload.
///
/// Longer than any file path but short enough to trim multi-line code
/// content from `tool_input` and `tool_result`.
const HOST_PAYLOAD_STRING_MAX: usize = 512;

/// Prepare the host CLI JSON for inclusion in the IPC request.
///
/// Clones the payload and truncates string values at the first newline
/// or [`HOST_PAYLOAD_STRING_MAX`] characters to prevent oversized
/// payloads from `tool_input` and `tool_result` fields (Edit/Write
/// hooks can include full file content).
fn prepare_host_payload(hook_json: &serde_json::Value) -> serde_json::Value {
    let mut payload = hook_json.clone();
    truncate_host_strings(&mut payload, HOST_PAYLOAD_STRING_MAX);
    payload
}

/// Truncate string values in a JSON tree to prevent oversized payloads.
///
/// String values are truncated at the first newline or `max_chars`
/// characters (whichever comes first). This preserves file paths and
/// identifiers while trimming multi-line code content.
fn truncate_host_strings(value: &mut serde_json::Value, max_chars: usize) {
    match value {
        serde_json::Value::String(s) => {
            // Find the byte offset to truncate at: first newline or
            // the max_chars-th character, whichever comes first.
            let mut byte_offset = None;
            for (i, (pos, ch)) in s.char_indices().enumerate() {
                if ch == '\n' || i >= max_chars {
                    byte_offset = Some(pos);
                    break;
                }
            }
            if let Some(pos) = byte_offset {
                s.truncate(pos);
                s.push('…');
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                truncate_host_strings(v, max_chars);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                truncate_host_strings(v, max_chars);
            }
        }
        _ => {}
    }
}

// ── Hook transport functions ────────────────────────────────────────────

/// Clear all editing state for a session (`SessionStart` hook handler).
///
/// Called on session start, resume, `/clear`, and `/compact`. The agent's
/// context is gone, so stale editing state must be cleared. No diagnostics
/// are delivered.
///
/// Also validates the configuration at session start. If the config is
/// invalid, surfaces a fresh `systemMessage` directing the user to
/// `catenary doctor` — a synchronous, error-severity notice on the right
/// surface (the notification queue retired in tui-rework 04; this direct check
/// is not queue-fed, so it never delivers stale).
pub fn run_session_start(format: HostFormat) {
    use crate::hook::response::SystemMessageBuilder;
    use crate::logging::Severity;

    // OpenCode (ws36 ticket 02): the plugin's `config` hook registers a
    // runtime-regenerated instructions file, and `catenary hook session-start
    // --format=opencode` is how it obtains the payload. Emit the raw teaching
    // body to stdout — the plugin captures stdout verbatim and writes it to that
    // file, so this is plain text, not the Claude `hookSpecificOutput` envelope.
    // No stdin read: the body is the live runtime projection (`emitted_payload`
    // resolves the commands surface against this process's cwd and prepends the
    // daemon-staleness note via the `tool/version` probe when the daemon is
    // stale), and OpenCode re-reads the file every prompt step, so it rides every
    // request with zero per-request plugin work.
    if matches!(format, HostFormat::OpenCode) {
        print!("{}", opencode_session_start_body());
        return;
    }

    let mut builder = SystemMessageBuilder::new();

    // Config validation — runs before IPC, no session needed.
    if let Err(e) = crate::config::Config::check() {
        builder.push_direct(
            Severity::Error,
            &format!("Catenary configuration error: {e:#}. Run `catenary doctor` for details."),
        );
    }

    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        emit_session_start(builder, None);
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        emit_session_start(builder, None);
        return;
    };

    // Source gating (ws36 ticket 01): inject the teaching payload unless the
    // context provably already carries it (`resume`). `source` is read here,
    // CLI-side, from the full hook payload; the payload itself is resolved live
    // (`session_start_context`) so its commands surface reflects this session.
    let source = hook_json.get("source").and_then(|v| v.as_str());
    let announce = session_start_should_announce(source);

    let Some(stream) = hook_connect(&hook_json) else {
        let ctx =
            with_bridge_mismatch_line(with_orphan_line(session_start_context(announce, format)));
        emit_session_start(builder, ctx.as_deref());
        return;
    };

    let session_id = extract_session_id(&hook_json, format);
    let mut request = serde_json::json!({
        "method": "session-start/clear-editing",
        "format": format.as_str(),
    });
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }
    request["host_payload"] = prepare_host_payload(&hook_json);

    // Fire the clear-editing request for its daemon-side effect (resetting stale
    // editing state). The response carries the project-config setup nudge (misc
    // 202) when the daemon owes one for a served root this SessionStart — the
    // daemon owns the once-per-root-per-instance ledger, so the CLI only renders
    // what it is handed. The Cleared count retired (tui-rework 04); nothing else in
    // the response is user-facing.
    let lines = ipc_exchange(stream, &request);
    let nudge = session_start_nudge_from_response(&lines);

    let ctx = with_project_config_line(
        with_bridge_mismatch_line(with_orphan_line(session_start_context(announce, format))),
        nudge.as_deref(),
    );
    emit_session_start(builder, ctx.as_deref());
}

/// The `session_start_nudge` line the daemon returned in its
/// `session-start/clear-editing` response, or `None` (misc 202).
///
/// The daemon emits at most one nudge per served root per instance; the CLI reads
/// it off the first response line and renders it verbatim. An absent field (the
/// common case — no nudge owed, or the root already nudged) leaves this `None`.
#[must_use]
fn session_start_nudge_from_response(lines: &[String]) -> Option<String> {
    let line = lines.first()?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value
        .get("session_start_nudge")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Append the project-config setup-nudge line (misc 202) to a `SessionStart`
/// context, when the daemon returned one and there is a context to carry it.
///
/// Mirrors [`with_orphan_line`] / [`with_bridge_mismatch_line`] exactly: it only
/// augments a `Some` context (the announce+Claude path — the sole `SessionStart`
/// surface that carries `additionalContext`), so a non-Claude host never receives
/// a Claude-shaped standalone line. A `None` context or an absent nudge is the
/// identity.
#[must_use]
fn with_project_config_line(ctx: Option<String>, nudge: Option<&str>) -> Option<String> {
    let base = ctx?;
    match nudge {
        Some(line) => Some(format!("{base}\n\n{line}")),
        None => Some(base),
    }
}

/// Inject the per-session teaching sliver on a conversation's first sighting
/// (Antigravity `PreInvocation` hook handler, teaching-surface ticket 03; sliver
/// as of ticket 14).
///
/// Antigravity has no `SessionStart` surface; its `PreInvocation` hook fires
/// before **every** model call carrying `invocationNum` and `conversationId`.
/// Rather than inject per-call (the `ephemeralMessage` channel is transient and
/// excluded by ruling), this delivers, once per conversation, the per-session
/// **sliver** ([`crate::cli::teaching::session_sliver`]) as a single
/// **persisted** `injectSteps` `userMessage`. The always-on rules file already
/// carries the workspace-invariant surface every turn (teaching-surface ticket
/// 14), so the injection carries only the session-specific delta the rules file
/// structurally cannot — the cwd build tool. When the cwd resolves no build tool
/// there is no delta, so nothing is injected (the rules file has it all).
///
/// First-sighting is decided **daemon-side**, keyed on `conversationId`, not by
/// the stateless `invocationNum == 0` trigger: the daemon already sees
/// Antigravity's hooks, and a per-conversation ledger is robust to whatever
/// `invocationNum` does on resume (a resumed conversation restores its
/// transcript — including the persisted `userMessage` — so re-injecting would
/// duplicate it). Fail-closed when the daemon is unreachable: the ledger only
/// records a sighting when the daemon actually answers, so a skipped injection
/// self-heals on the next reachable model call rather than risking a duplicate.
///
/// Only Antigravity registers this hook; any other host is a defensive no-op.
pub fn run_pre_invocation(format: HostFormat) {
    if !matches!(format, HostFormat::Antigravity) {
        print!("{}", empty_pre_invocation());
        return;
    }

    // Teaching-surface 12: the Antigravity rules file is re-injected per turn, so
    // a `PreInvocation` firing means "Antigravity is active" — regenerate the
    // installed rules file to the live workspace-invariant surface. Hash-gated so
    // this per-model-call path is render + read + compare; fail-open so it never
    // blocks the first-sighting injection below (see `context_files`).
    crate::cli::context_files::regenerate_antigravity_rules();

    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        print!("{}", empty_pre_invocation());
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        print!("{}", empty_pre_invocation());
        return;
    };

    // Inject only on the first sighting, and only when there is a session-specific
    // delta to carry — the rules file already delivers the shared surface. The
    // sliver render is computed only on the first sighting, so the per-model-call
    // hot path pays only the ledger round-trip, not a second config load.
    if pre_invocation_first_sighting(&hook_json, format)
        && let Some(sliver) = crate::cli::teaching::session_sliver()
    {
        print!("{}", pre_invocation_injection(&sliver));
    } else {
        print!("{}", empty_pre_invocation());
    }
}

/// Ask the daemon whether this `PreInvocation` is the conversation's first
/// sighting, returning `true` iff the teaching payload should be injected now.
///
/// The daemon owns the ledger (keyed on `conversationId`) and atomically
/// check-and-records, so exactly one `PreInvocation` per conversation is told to
/// inject. Fail-closed: a missing `conversationId` or an unreachable /
/// unanswering daemon yields `false` (no injection), which self-heals on the
/// next reachable call because the daemon recorded nothing.
fn pre_invocation_first_sighting(hook_json: &serde_json::Value, format: HostFormat) -> bool {
    let Some(session_id) = extract_session_id(hook_json, format) else {
        return false;
    };
    let Some(stream) = hook_connect(hook_json) else {
        return false;
    };
    let request = serde_json::json!({
        "method": "pre-invocation/first-sighting",
        "format": format.as_str(),
        "session_id": session_id,
    });
    ipc_exchange(stream, &request)
        .first()
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|v| v.get("inject").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Clean up session state on exit (`SessionEnd` hook handler).
///
/// Forwards the session-end signal to the daemon so it can remove
/// the session's root contributions from the refcount tracker.
/// Best effort — the host CLI will not wait for completion and
/// ignores all flow-control fields.
pub fn run_session_end(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let session_id = extract_session_id(&hook_json, format);
    let mut request = serde_json::json!({
        "method": "session-end/cleanup",
        "format": format.as_str(),
    });
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }

    // Fire and forget — no response processing needed.
    let _ = ipc_exchange(stream, &request);
}

/// Mount a subagent's worktree as a workspace root and inject the Catenary
/// teaching payload (`SubagentStart` hook handler).
///
/// Fires once at subagent spawn. Two **decoupled** effects:
/// - **Mount (conditional):** forwards `cwd` (the worktree of an
///   `isolation:"worktree"` subagent) and `session_id` to the daemon, which
///   mounts the worktree under `worktree:{session_id}:{path}` iff its canonical
///   project root is already tracked (the Explore/Plan self-scoping is enforced
///   daemon-side). Best effort — the host CLI ignores all flow-control fields.
/// - **Teaching payload (unconditional, ws36 ticket 01):** every subagent spawn
///   is a fresh context that never had the payload, so the shared teaching body
///   plus a per-agent debt line is emitted via
///   `hookSpecificOutput.additionalContext` for **every** subagent, regardless
///   of whether a worktree was mounted. This is a Claude Code channel; no other
///   supported host spawns subagents.
pub fn run_subagent_start(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        emit_subagent_start_announcement(format);
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        emit_subagent_start_announcement(format);
        return;
    };

    // Mount (conditional): best-effort daemon round-trip. Decoupled from the
    // payload injection below — most subagents mount nothing but all are taught.
    if let Some(stream) = hook_connect(&hook_json) {
        let mut request = serde_json::json!({
            "method": "subagent-start/mount-worktree",
            // Forward the agent identity for the subagent BOARD row and the
            // dispose ownership tag (the worktree's dirname). It no longer keys
            // the mount — mounts are uniformly path-keyed
            // (`worktree:{session_id}:{canonical-path}`; root-ownership 04), and
            // the SubagentStop reap resolves by cwd, not identity.
            "agent_id": extract_agent_id(&hook_json),
            "format": format.as_str(),
        });
        if let Some(sid) = extract_session_id(&hook_json, format) {
            request["session_id"] = serde_json::json!(sid);
        }
        if let Some(cwd) = extract_cwd_str(&hook_json, format) {
            request["cwd"] = serde_json::json!(cwd);
        }
        // Forward the full host payload (symmetry with `post-agent`), the live
        // drift-net confirming SubagentStart's schema on the first real run.
        request["host_payload"] = prepare_host_payload(&hook_json);

        // Fire and forget — no response processing needed.
        let _ = ipc_exchange(stream, &request);
    }

    emit_subagent_start_announcement(format);
}

/// Emit the unconditional `SubagentStart` teaching payload.
///
/// Prints a single object carrying `hookSpecificOutput.additionalContext` for
/// Claude; other hosts get nothing (no other supported host spawns subagents).
fn emit_subagent_start_announcement(format: HostFormat) {
    if let Some(obj) = build_subagent_start_response(format) {
        print!("{obj}");
    }
}

/// Build the `SubagentStart` hook-response object carrying the teaching
/// payload, or `None` for non-Claude hosts.
///
/// The payload is the shared body plus a per-agent debt line, with the
/// daemon-staleness note prepended when the daemon is stale
/// ([`crate::cli::teaching::emitted_subagent_payload`]) — self-contained and
/// prefix-identifiable, since a subagent's `additionalContext` lands in its own
/// window under one shared label alongside other hooks' context.
fn build_subagent_start_response(format: HostFormat) -> Option<serde_json::Value> {
    if !matches!(format, HostFormat::Claude) {
        return None;
    }
    let ctx = crate::cli::teaching::emitted_subagent_payload();
    Some(serde_json::json!({
        "hookSpecificOutput": announcement_hook_specific_output("SubagentStart", &ctx),
    }))
}

/// Tear down a subagent's worktree root (`WorktreeRemove` hook handler).
///
/// Fires once at the true removal of an `isolation:"worktree"` subagent's
/// worktree. Forwards `worktree_path` and `session_id` to the daemon, which
/// removes the `worktree:{session_id}:{path}` contributor so the worktree's
/// rust-analyzer shuts down. Best effort — the host CLI ignores all
/// flow-control fields.
pub fn run_worktree_remove(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let mut request = serde_json::json!({
        "method": "worktree-remove/unmount-worktree",
        "format": format.as_str(),
    });
    if let Some(sid) = extract_session_id(&hook_json, format) {
        request["session_id"] = serde_json::json!(sid);
    }
    if let Some(path) = extract_worktree_path(&hook_json, format) {
        request["worktree_path"] = serde_json::json!(path);
    }

    // Fire and forget — no response processing needed.
    let _ = ipc_exchange(stream, &request);
}

/// Answer a read-class permission prompt so the agent never parks on it
/// (`PermissionRequest` hook handler — the answer desk, misc 201 / decision 031).
///
/// Forwards `session_id` + `agent_id` + the **full** host payload (`tool_name`,
/// `tool_input`, cwd) to the daemon, which computes the read policy (decision 031,
/// "Reads nod through, writes wait") and — same round-trip — marks the worktree
/// root enclosing the prompt's cwd blocked for `catenary worktree ls`. The daemon
/// returns the decision JSON, which this prints verbatim:
///
/// - **Deny** (a sensitive file) with the maintainer's teaching — a deny does not
///   park the agent; the model gets the reason and continues.
/// - **Allow** with the resolved realpath pinned via `updatedInput` (quiet inside
///   declared scope; loud + recorded outside; `always_read` also promotes the
///   enclosing prefix into the session's working directories on the first allow).
/// - **Nothing** for a write-class tool — the human decides.
///
/// **Fail-PASS** (the load-bearing safety rule): only Claude Code's
/// `PermissionRequest` is answered, and only when the daemon actually returns a
/// non-empty decision. An unreachable daemon, a non-Claude host, or an
/// unresolvable target prints nothing — the human's prompt must stand; the desk
/// never eats a prompt it cannot answer.
pub fn run_permission_request(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    // Only Claude Code carries the answer-desk PermissionRequest surface; any
    // other host is a no-op (nothing printed — fail PASS).
    if !matches!(format, HostFormat::Claude) {
        return;
    }

    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let mut request = serde_json::json!({
        "method": "permission-request/blocked",
        "agent_id": extract_agent_id(&hook_json),
        "format": format.as_str(),
    });
    if let Some(sid) = extract_session_id(&hook_json, format) {
        request["session_id"] = serde_json::json!(sid);
    }
    // The desk needs the tool_name + tool_input to classify the read, so forward
    // the FULL raw payload (not the truncated one — a path could exceed the
    // capture cap and mis-resolve). The blocked-marking cwd rides along with it.
    request["host_payload"] = hook_json;

    // The daemon returns the decision JSON (or an empty line for "no decision").
    // Print it verbatim; an empty line prints nothing — the human's prompt stands.
    for line in ipc_exchange(stream, &request) {
        if !line.trim().is_empty() {
            print!("{line}");
        }
    }
}

/// Reserved no-op shim shared by every hook event registered ahead of its
/// behavior (full-surface registration, pre-v2 maintainer ruling).
///
/// The entire hook-event surface of each host is wired in its hooks.json so
/// future behavioral changes land in the binary — which the host already
/// invokes — without another hooks.json churn (each hooks.json change is a
/// stale transition requiring `catenary install <host>`, because the host runs
/// hooks from a frozen cache copy). Events with no behavior yet terminate
/// here: drain stdin to EOF and drop it (the host writes the event payload to
/// the hook's stdin — exiting without draining risks a host-side EPIPE), then
/// answer in the host's dialect: Claude Code tolerates silence, so its shims
/// emit nothing; Antigravity's contract is JSON-in/JSON-out ("hooks … should
/// return output via stdout as JSON"), so its shims answer the documented
/// empty object `{}`. Hard constraints, since this runs on every event
/// occurrence: no daemon connection, no logging, no output beyond the
/// dialect's empty answer, and success even on malformed or empty stdin.
/// Observability wiring is post-v2.
pub fn run_reserved_shim(format: HostFormat) {
    drain_hook_stdin(std::io::stdin().lock());
    if matches!(format, HostFormat::Antigravity) {
        print!("{{}}");
    }
}

/// Read a hook payload stream to EOF and discard it, returning the number of
/// bytes drained.
///
/// A byte-level copy into [`std::io::sink`]: the payload's content is
/// irrelevant — garbage and invalid UTF-8 drain exactly like well-formed JSON;
/// only reaching EOF matters. Read errors are swallowed (yielding the bytes
/// drained before the error): the shim's contract is silent success.
fn drain_hook_stdin(mut reader: impl std::io::Read) -> u64 {
    std::io::copy(&mut reader, &mut std::io::sink()).unwrap_or(0)
}

/// Create an out-of-tree agent worktree (`WorktreeCreate` hook handler).
///
/// Unlike every other hook — which fail *open* so the host CLI's flow is never
/// broken — `WorktreeCreate` owns worktree creation under Claude Code's
/// success/failure contract: on success the created worktree's absolute path is
/// the **only** stdout output; any failure must exit nonzero so the host fails
/// worktree creation. This function therefore returns a `Result` (the caller in
/// `main` prints the error to stderr and exits nonzero); the path print is the
/// sole `print!` on the success path.
///
/// The stdin payload is parsed **leniently** (a bare `serde_json::Value`, so
/// unknown/extra fields are tolerated) and the full payload is debug-logged plus
/// best-effort forwarded to the daemon firehose (`catenary query --kind hook`),
/// so the first live run reveals Claude Code's actual schema — the safety net
/// for doc drift. Only `cwd` is required (to locate the source repo); the
/// branch name is taken from a payload-supplied name when present, else
/// generated.
///
/// # Errors
///
/// Returns an error when stdin is unreadable/unparseable, when no source repo
/// can be resolved from the payload, or when `git worktree add` fails.
pub fn run_worktree_create(format: HostFormat) -> anyhow::Result<()> {
    use anyhow::Context;

    let stdin_data =
        std::io::read_to_string(std::io::stdin()).context("read WorktreeCreate stdin")?;
    let hook_json = serde_json::from_str::<serde_json::Value>(&stdin_data)
        .context("parse WorktreeCreate payload as JSON")?;

    // Log the FULL payload so the first live run verifies Claude Code's schema
    // (the docs do not pin down every field). Emitted locally at debug and — best
    // effort — forwarded to the daemon so it lands in the queryable firehose.
    tracing::debug!(
        source = crate::source::Source::HookDispatch.as_str(),
        payload = %hook_json,
        "WorktreeCreate payload received",
    );

    let meta = crate::worktree_create::create_from_payload(&hook_json)?;

    // Register the created worktree with the daemon (the in-memory half of the
    // registry; the sidecar is the durable half). Best-effort — never affects
    // the creation contract.
    forward_worktree_create_payload(&hook_json, &meta, format);

    // The stdout contract: the created worktree's absolute path, and nothing
    // else. Uses the same bare `print!` (no trailing newline) the other hook
    // subcommands use for stdout — `println!` is denied by the house rules.
    print!("{}", meta.worktree.display());
    Ok(())
}

/// Best-effort forward of the `WorktreeCreate` registration to the daemon.
///
/// Two purposes, one round-trip: the full host payload lands in the JSONL
/// firehose (`catenary query --kind hook`, the live schema-verification surface),
/// and the [`WorktreeMeta`](crate::worktree_create::WorktreeMeta) registers the
/// identity→path map in the daemon registry (misc 150). Silently skipped when
/// the daemon is unreachable and its result is ignored, so it can never affect
/// worktree creation (whose success/failure is the host contract).
fn forward_worktree_create_payload(
    hook_json: &serde_json::Value,
    meta: &crate::worktree_create::WorktreeMeta,
    format: HostFormat,
) {
    let Some(stream) = hook_connect(hook_json) else {
        return;
    };
    let mut request = serde_json::json!({
        "method": "worktree-create/log-payload",
        "format": format.as_str(),
    });
    if let Some(sid) = extract_session_id(hook_json, format) {
        request["session_id"] = serde_json::json!(sid);
    }
    request["host_payload"] = prepare_host_payload(hook_json);
    request["worktree_meta"] = serde_json::to_value(meta).unwrap_or(serde_json::Value::Null);
    let _ = ipc_exchange(stream, &request);
}

/// Build the single `SessionStart` hook-response object.
///
/// Carries up to two coexisting surfaces in **one** JSON object:
/// - a top-level `systemMessage` (the user-facing notification drain), present
///   when the builder has content;
/// - `hookSpecificOutput.additionalContext` (the silent teaching payload),
///   present when `additional_context` is `Some` — the caller
///   ([`session_start_context`]) already gates it on the inject `source` and the
///   host (Claude Code reads `additionalContext` at `SessionStart`).
///
/// Returns `None` when neither surface has content, so nothing is emitted.
fn build_session_start_response(
    builder: crate::hook::response::SystemMessageBuilder,
    additional_context: Option<&str>,
) -> Option<serde_json::Value> {
    let system_message = builder.finish();

    if system_message.is_none() && additional_context.is_none() {
        return None;
    }

    let mut obj = serde_json::Map::new();
    if let Some(msg) = system_message {
        obj.insert("systemMessage".to_string(), serde_json::Value::String(msg));
    }
    if let Some(ctx) = additional_context {
        obj.insert(
            "hookSpecificOutput".to_string(),
            announcement_hook_specific_output("SessionStart", ctx),
        );
    }
    Some(serde_json::Value::Object(obj))
}

/// Finalize and print the `SessionStart` hook response (notification drain +
/// teaching payload) as a single JSON object.
///
/// The host gate lives in [`session_start_context`], which yields `Some`
/// context only for the host whose `SessionStart` carries `additionalContext`
/// (Claude Code) — so this needs no `format`.
fn emit_session_start(
    builder: crate::hook::response::SystemMessageBuilder,
    additional_context: Option<&str>,
) {
    if let Some(obj) = build_session_start_response(builder, additional_context) {
        print!("{obj}");
    }
}

/// Format a parent-agent `additionalContext` payload as a hook response for the
/// given hook event (misc 151, D-1).
///
/// `additionalContext` is Claude Code's agent-context channel. Other hosts have
/// no equivalent field, and the dirty-worktree notice that populates the queue
/// only originates from Claude Code's `Worktree`/`Subagent` flow, so this emits
/// an empty response (allow, nothing added) for non-Claude hosts.
fn format_additional_context(context: &str, hook_event_name: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude => serde_json::json!({
            "hookSpecificOutput": announcement_hook_specific_output(hook_event_name, context),
        })
        .to_string(),
        HostFormat::Antigravity | HostFormat::OpenCode => String::new(),
    }
}

/// Force `done_editing` before the agent finishes responding (`Stop` / `AfterAgent`
/// hook handler).
///
/// If the agent has files in editing state, blocks the stop with a message
/// directing the agent to call `done_editing`. If `stop_hook_active` is true
/// (retry after agent failed to comply), force-clears the stale editing state
/// and allows the stop.
pub fn run_post_agent(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let stop_hook_active = hook_json
        .get("stop_hook_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let agent_id = extract_agent_id(&hook_json);
    let session_id = extract_session_id(&hook_json, format);

    let mut request = serde_json::json!({
        "method": "post-agent/require-release",
        "agent_id": agent_id,
        "stop_hook_active": stop_hook_active,
        "format": format.as_str(),
    });
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }
    request["host_payload"] = prepare_host_payload(&hook_json);

    let lines = ipc_exchange(stream, &request);

    if let Some(line) = lines.first()
        && let Ok(envelope) = serde_json::from_str::<crate::hook::HookResponseEnvelope>(line)
        && let Some(crate::hook::HookResult::Block(reason)) = &envelope.result
    {
        print!("{}", format_stop_block(reason, format));
    }
}

/// The redirect-to-`catenary` teaching for a denied host `Grep`/`Glob` tool, or
/// `None` when the tool is not a host search tool or its lever is off (misc 201,
/// component 2).
///
/// Voice-matches the shell-side `rg`/`find` denial
/// ([`crate::cli::command_filter`], the `Redirect` guidance opening line): "`X`
/// isn't allowed. Use `catenary Y` instead. Works on any path (LSP enrichment
/// only within tracked roots)." A config that won't load fails OPEN (the lever is
/// treated as off) so a broken config never denies a search tool the user did not
/// ask to deny.
#[must_use]
fn host_search_tool_denial(tool_name: &str) -> Option<String> {
    // A host search tool at all? (Cheap check before the config load.)
    if !matches!(tool_name, "Grep" | "Glob") {
        return None;
    }
    let config = crate::config::Config::load().ok()?;
    host_search_tool_denial_for(tool_name, &config.permissions())
}

/// Pure core of [`host_search_tool_denial`]: the redirect-to-`catenary` teaching
/// for a denied host `Grep`/`Glob` tool given the resolved permission policy, or
/// `None` when the tool is not a host search tool or its lever is off.
///
/// Split from the config-loading wrapper so the deny message and the lever
/// polarity are unit-testable without touching the real user config.
#[must_use]
fn host_search_tool_denial_for(
    tool_name: &str,
    permissions: &crate::config::PermissionsConfig,
) -> Option<String> {
    let redirect = match tool_name {
        "Grep" if permissions.deny_host_grep => "grep",
        "Glob" if permissions.deny_host_glob => "glob",
        _ => return None,
    };
    Some(format!(
        "`{tool_name}` isn't allowed. Use `catenary {redirect}` instead. Works on any path (LSP enrichment only within tracked roots)."
    ))
}

/// Editing state enforcement and command filtering (`PreToolUse` / `BeforeTool`
/// hook handler).
///
/// Checks shell commands against the configured allowlist (client-side),
/// then forwards to the session for editing state enforcement. When a
/// command is denied, queries the session for debounce state to decide
/// between a full config dump or a short message. Falls back to a static
/// full dump if the session is unreachable.
///
/// Silently succeeds on any error to avoid breaking the host CLI's flow.
pub fn run_pre_tool(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let tool_name = extract_tool_name(&hook_json, format);

    // ── Host Grep/Glob deny levers (misc 201, component 2) ──
    // When `[permissions] deny_host_grep`/`deny_host_glob` is set, the host's
    // built-in Grep/Glob tools are denied with the same redirect-to-`catenary`
    // teaching the shell-side `rg`/`find` denial uses, so users stop carrying the
    // denial in client settings. Today the PreToolUse hook only acts on shell and
    // edit tools — these levers close the host-tool gap. Checked before anything
    // else so a denied host tool never falls through to editing-state.
    if let Some(reason) = host_search_tool_denial(tool_name) {
        print!("{}", format_deny(&reason, format));
        return;
    }

    // ── Catenary CLI commands (regime 1: canonical-form matcher) ──
    // Recognize and classify Catenary's own subcommands in any position
    // (ADR 013). `grep`/`glob`/`sed`/`diagnostics`/`roots`/`primer` are
    // allowed only in canonical form (no pipe/redirect/substitution/
    // background; `diagnostics`/`sed` and the editing lifecycle must be
    // bare); non-agent commands and unrecognized subcommands are denied
    // with a pedagogical message.
    if let Some(ref shell_cmd) = extract_shell_command(&hook_json, tool_name, format) {
        use crate::cli::command_filter::CatenaryAction;
        // The declared client keys client-specific denials (misc 177): the
        // format travels with the hook definition, so the capability it
        // implies (a WorktreeCreate registration) holds by construction.
        match crate::cli::command_filter::analyze_catenary_command(shell_cmd, Some(format)) {
            CatenaryAction::Deny(reason) => {
                print!("{}", format_deny(&reason, format));
                return;
            }
            // `editing start` sends IPC to the daemon, then allows the command
            // to run. `diagnostics` stages nothing (root-ownership stage 3 retired
            // the prepare handoff) — its hook only runs the owner gate, then
            // allows so the CLI serves against the ledger.
            CatenaryAction::EditingStart => {
                handle_start_editing_hook(&hook_json, format);
                return;
            }
            CatenaryAction::Diagnostics => {
                handle_done_editing_hook(&hook_json, Some(shell_cmd.as_str()), format);
                return;
            }
            CatenaryAction::Claim => {
                handle_claim_hook(&hook_json, shell_cmd, format);
                return;
            }
            // Canonical search/tool command. A `cd`/foreign-chained search
            // (or an arg-substitution carrying a foreign command) still has
            // its foreign segments allowlist-checked (regime 2); `catenary`
            // segments are skipped by `check_command`. Always allowed,
            // including during editing, so we never fall through to
            // editing-state enforcement.
            CatenaryAction::Allow { .. } => {
                // Every allowed catenary line resolves its write-set and is
                // opaque-gated through the same filter — including catenary-only
                // redirects (`catenary grep p > f`), the folded-in edge of ws38
                // ticket 02. A search chain's foreign segments
                // (`cd src && catenary grep p`) stay allowlist-checked;
                // `catenary` segments are skipped. On allow, the resolved
                // write-set is attributed to the caller like an edit.
                match foreign_command_outcome(&hook_json, shell_cmd, format) {
                    Err(reason) => print!("{}", format_deny(&reason, format)),
                    Ok(writes) if !writes.is_empty() => enforce_editing_state(
                        &hook_json,
                        tool_name,
                        None,
                        Some(shell_cmd.as_str()),
                        format,
                        &writes,
                    ),
                    Ok(_) => {}
                }
                return;
            }
            CatenaryAction::NotCatenary => {}
        }
    }

    // ── Command filter (regime 2: foreign allowlist) + write resolution ──
    // Local-only enforcement: the client-side check (user config + cwd's
    // project config) is the sole authority — enforcement keys are
    // user-level, so it reaches the same verdict as any daemon round-trip
    // would, and the local cwd is more accurate than the daemon's view. On
    // allow the resolver's write-set rides into editing-state so covered
    // targets are attributed to the caller like edits (ws38 ticket 02).
    let mut resolved_writes: Vec<PathBuf> = Vec::new();
    if let Some(shell_cmd) = extract_shell_command(&hook_json, tool_name, format) {
        // Section-scoped quarantine (bug 110): a `[commands]` that failed
        // validation must degrade LOUDLY, never silently. `commands_quarantine_verdict`
        // fires the once-per-config-mtime desktop notification and decides the
        // stance. `None` ⇒ clean config, proceed with normal enforcement.
        if let Some(verdict) = commands_quarantine_verdict() {
            match verdict {
                // Fail-closed opt-in: deny this (necessarily non-catenary — regime
                // 1 already returned for catenary commands) command with a teaching
                // message, on every call.
                QuarantineVerdict::Deny(reason) => {
                    print!("{}", format_deny(&reason, format));
                    return;
                }
                // Fail-open: enforcement is off; the command is allowed. On the
                // onset, tell the agent via `additionalContext` (one line, then
                // straight to editing-state so file tracking is unaffected). The
                // resolver never ran, so there is no write-set to attribute.
                QuarantineVerdict::AllowWithContext { context } => {
                    if let Some(ctx) = context {
                        print!("{}", format_additional_context(&ctx, "PreToolUse", format));
                        return;
                    }
                }
            }
        } else {
            match foreign_command_outcome(&hook_json, &shell_cmd, format) {
                Err(reason) => {
                    print!("{}", format_deny(&reason, format));
                    return;
                }
                Ok(writes) => resolved_writes = writes,
            }
        }
    }

    // ── Editing state enforcement (IPC to daemon / session) ──────
    let file_path = extract_file_path(&hook_json, format);
    let shell_cmd = extract_shell_command(&hook_json, tool_name, format);
    enforce_editing_state(
        &hook_json,
        tool_name,
        file_path.as_deref(),
        shell_cmd.as_deref(),
        format,
        &resolved_writes,
    );
}

/// Send the `pre-tool/editing-state` request to the daemon and print any
/// denial it returns.
///
/// The universal pre-tool transport: editing-state enforcement for every tool,
/// plus (ws38 ticket 02) the resolved shell-write set carried in `writes` so
/// the daemon attributes those targets into the caller's modified-set exactly
/// like an Edit/Write. Silently no-ops when the daemon socket is unreachable
/// (the host CLI's flow must not break on a hook transport error).
fn enforce_editing_state(
    hook_json: &serde_json::Value,
    tool_name: &str,
    file_path: Option<&str>,
    shell_cmd: Option<&str>,
    format: HostFormat,
    writes: &[PathBuf],
) {
    // ── Durable root lock (root-ownership stage 2) ──────────────────────
    // Acquire (or re-affirm) the per-root lock at the edit seam BEFORE any
    // daemon contact, so the one-cook-per-kitchen rule holds with the daemon
    // down. A collision denies with the briefing and accumulates nothing (the
    // guardrail-deny invariant); an allow (ours / foreign / uncovered) flows on
    // to the daemon editing-state transport below. Read-only tools carry no
    // edited path and take no lock — they look through the window unbounded.
    if let Some(reason) = root_lock_gate(hook_json, tool_name, file_path, shell_cmd, format, writes)
    {
        print!("{}", format_deny(&reason, format));
        return;
    }

    let Some(stream) = hook_connect(hook_json) else {
        return;
    };

    let agent_id = extract_agent_id(hook_json);
    let session_id = extract_session_id(hook_json, format);

    let mut request = serde_json::json!({
        "method": "pre-tool/editing-state",
        "tool_name": tool_name,
        "agent_id": agent_id,
        "format": format.as_str(),
    });
    if let Some(path) = file_path {
        request["file_path"] = serde_json::json!(path);
    }
    if let Some(cmd) = shell_cmd {
        request["command"] = serde_json::json!(cmd);
    }
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }
    // Forward the host's cwd (root-ownership stage 3): the daemon-side Bash nag
    // resolves it to the enclosing lock root and reads that root's ledger to
    // answer "unpaid debt?", so a fresh daemon re-arms the nag against the
    // durable ledger (debt outlives daemon churn).
    if let Some(cwd) = extract_cwd_str(hook_json, format) {
        request["cwd"] = serde_json::json!(cwd);
    }
    if !writes.is_empty() {
        request["writes"] = serde_json::json!(writes);
    }
    request["host_payload"] = prepare_host_payload(hook_json);

    let lines = ipc_exchange(stream, &request);

    if let Some(line) = lines.first()
        && let Ok(envelope) = serde_json::from_str::<crate::hook::HookResponseEnvelope>(line)
        && let Some(crate::hook::HookResult::Deny(reason)) = &envelope.result
    {
        print!("{}", format_deny(reason, format));
    }
}

/// The durable-root-lock gate at the tool seam (root-ownership stages 2 + 5).
///
/// Hook-process-local: enforces the one-cook-per-kitchen rule across all three
/// command tiers (root-ownership stage 5), using the identity the host supplied
/// at THIS seam (`<client>+<session>+<agent>`, the one place identity appears).
/// The lock is a filesystem fact under `state_dir/locks/`, so it works with the
/// daemon down (unlike the daemon-side `EditingGuardrail`, which dies with the
/// daemon).
///
/// - **Edit tier** — the Edit/Write `file_path` and any resolved shell `writes`
///   [`acquire`](crate::lock::acquire) (or re-affirm) the per-root lock, booking
///   each covered target. A collision (another agent holds the root) denies with
///   the briefing.
/// - **Stateful tier** — a `build` / mutating-git-subcommand / `chmod` command
///   ([`tier::classify_command`](crate::cli::command_filter::tier::classify_command))
///   takes no file lock (it edits no named path), but a stateful operation in a
///   kitchen ANOTHER agent holds is the same trespass through a different door.
///   The command's root is resolved from cwd; if it is locked by another
///   identity, the command is denied with the same briefing shape as the edit
///   seam's.
/// - **Read tier** — everything else (reads, read-only git) passes unconditionally
///   — a waiter looks through the window unbounded.
///
/// The asymmetry that must hold: an UNLOCKED root imposes nothing (a lone cook
/// works free); only a root LOCKED BY ANOTHER identity denies. Booking is
/// static-data-driven ([`crate::lock::Booking`] from a hook-side `Config::load()`),
/// so no daemon connection is ever made here. A config that fails to load skips
/// the gate entirely (fail-open): a lone agent must never be false-denied because
/// the booking data was unreadable.
///
/// Returns `Some(briefing)` on the first collision, else `None` (the tool flows
/// on).
fn root_lock_gate(
    hook_json: &serde_json::Value,
    tool_name: &str,
    file_path: Option<&str>,
    shell_cmd: Option<&str>,
    format: HostFormat,
    writes: &[PathBuf],
) -> Option<String> {
    // Static booking data — no daemon. A config that won't load fails open (for
    // BOTH tiers: a lone agent is never false-denied on an unreadable config).
    let Ok(config) = crate::config::Config::load() else {
        return None;
    };
    let booking = crate::lock::Booking::from_config(&config);

    let owner = crate::lock::Owner::new(
        format.as_str(),
        extract_session_id(hook_json, format).unwrap_or_default(),
        extract_agent_id(hook_json),
    );
    let now = std::time::SystemTime::now();

    // ── Edit tier: acquire (and book) each covered edit target ──────────────
    // The Edit/Write path (only for an actual edit tool) plus every resolved
    // shell write. A collision denies; an allow (ours / foreign / uncovered)
    // flows on.
    let mut targets: Vec<PathBuf> = Vec::new();
    if crate::bridge::is_edit_tool(tool_name)
        && let Some(path) = file_path
    {
        targets.push(PathBuf::from(path));
    }
    targets.extend(writes.iter().cloned());
    for target in &targets {
        if let crate::lock::Acquired::Denied(briefing) =
            crate::lock::acquire(target, &owner, &booking, now)
        {
            return Some(briefing);
        }
    }

    // ── Stateful tier: a mutating kitchen operation needs the lock ──────────
    // A `build` / mutating-git / `chmod` command edits no named path, so the
    // edit-tier acquire above did not gate it. Resolve its root from cwd and deny
    // only when it is held by ANOTHER identity (an unlocked root imposes nothing).
    if let Some(cmd) = shell_cmd {
        let cwd = extract_cwd_str(hook_json, format).map(PathBuf::from);
        let resolved = config.resolved_commands.as_ref();
        if let Some(rules) = resolved
            && crate::cli::command_filter::tier::classify_command(
                cmd,
                writes,
                rules,
                cwd.as_deref(),
            )
            .requires_lock()
            && let Some(reason) = stateful_root_gate(cwd.as_deref(), &owner, now)
        {
            return Some(reason);
        }
    }

    None
}

/// Deny a stateful-tier command whose kitchen (the cwd's enclosing root) is held
/// by another identity (root-ownership stage 5).
///
/// Resolves the cwd's lock root and reads its owner file by pure path algebra. An
/// unlocked root (no lock dir) or one this caller owns imposes nothing — the
/// command flows on. Only a root LOCKED BY ANOTHER identity denies, with the same
/// briefing shape as the edit seam's (root path copy-pasteable, `catenary claim`
/// rescue, the softened paid-idle copy). An unresolvable cwd (scratch dir, no VCS
/// checkout) resolves to no root — nothing to gate.
fn stateful_root_gate(
    cwd: Option<&std::path::Path>,
    owner: &crate::lock::Owner,
    now: std::time::SystemTime,
) -> Option<String> {
    let cwd = cwd?;
    let root = crate::lock::resolve_lock_root(cwd)?;
    let holder = crate::lock::owner_of(&root)?;
    if &holder == owner {
        return None; // ours — a stateful op in our own kitchen is free
    }
    // Locked by another identity — deny with the edit-seam briefing shape.
    Some(crate::lock::holder_briefing(&root, now))
}

/// Run the foreign-command allowlist filter (regime 2) **and** the write
/// resolver, returning the resolved write-set on allow or the formatted denial
/// reason on deny.
///
/// `Ok(writes)` — the command is allowed; `writes` are the resolved (absolute
/// when the cwd is known) write targets to attribute into the caller's
/// modified-set (ws38 ticket 02). `Err(reason)` — the formatted denial to
/// print, for an allowlist violation or an opaque write.
///
/// The local [`check_shell_command`] is the sole authority: enforcement keys
/// (`allow`/`pipeline`/`deny`/`deny_flags`) are user-level, so the local path
/// reaches the same verdict as any daemon round-trip would, and the local cwd
/// is more accurate than the daemon's view. Catenary's own commands are skipped
/// by [`check_command`](crate::cli::command_filter::check_command) — they run
/// under the canonical-form matcher (regime 1), not the allowlist — but their
/// resolver-computed write-set still flows through here.
/// The verdict for a foreign command when `[commands]` is quarantined (bug 110).
///
/// The enforcement surface must degrade LOUDLY, never silently. By default the
/// section is treated as absent (fail-open — enforcement off); the
/// `client_enforcement_only = true` lever (recoverable best-effort from the
/// invalid section) flips this to fail-closed: non-catenary commands are denied
/// with a teaching message.
enum QuarantineVerdict {
    /// Fail-open: enforcement is off. `context` is `Some` only on the onset (the
    /// first hook per config mtime) — the `additionalContext` line telling the
    /// agent filtering is OFF — and `None` on every later hook, so the agent's
    /// context is not spammed once per tool call.
    AllowWithContext { context: Option<String> },
    /// Fail-closed (`client_enforcement_only = true`): deny with the teaching
    /// message naming the config error. Fires on EVERY call — a denied command
    /// must never slip through just because the onset notification already fired.
    Deny(String),
}

/// Reads `[commands] client_enforcement_only` best-effort from the raw config
/// document(s), even when the `[commands]` section is otherwise invalid (bug
/// 110).
///
/// The parsed [`ResolvedCommands`](crate::config::ResolvedCommands) is gone once
/// `[commands]` is quarantined, so the fail-closed opt-in is recovered by
/// re-reading the raw TOML: the last source that sets the boolean wins (mirroring
/// the layered merge). A file that fails to read or parse contributes nothing.
fn raw_client_enforcement_only() -> bool {
    let mut flag = false;
    for source in crate::config::config_sources() {
        let Ok(contents) = std::fs::read_to_string(&source) else {
            continue;
        };
        let Ok(raw) = toml::from_str::<toml::Value>(&contents) else {
            continue;
        };
        if let Some(value) = raw
            .get("commands")
            .and_then(|c| c.get("client_enforcement_only"))
            .and_then(toml::Value::as_bool)
        {
            flag = value;
        }
    }
    flag
}

/// Whether this hook is the onset for the current config mtime, firing the
/// once-per-config-mtime desktop notification as a side effect (bug 110).
///
/// The `PreToolUse` hook is one short-lived process per tool call, so the
/// interrupt is deduped across invocations by a [`QuarantineStamp`](crate::notify::QuarantineStamp)
/// keyed to the config file's mtime — the first hook after the config broke fires
/// one notification; later hooks against the same config stay silent. Returns
/// `true` on that onset so the caller also emits the agent-facing
/// `additionalContext` once, not per call. The loudest honest channel the hook
/// process has: its tracing subscriber only fires desktop notifications at
/// `error!()` severity (and an `error!()` here would re-fire on every process,
/// its per-process debounce useless across hooks), so the notification goes
/// point-blank through [`notify_desktop`](crate::notify::notify_desktop), which
/// respects `CATENARY_NOTIFY` and records intent under `CATENARY_NOTIFY_LOG`.
fn commands_quarantine_onset(summary: &str) -> bool {
    let Some(config_path) = crate::config::config_sources().into_iter().next() else {
        // No config file to key the stamp to — treat every sighting as an onset
        // (a lost warning about broken enforcement is worse than a duplicate).
        crate::notify::notify_desktop(
            "Catenary command filtering is OFF",
            &format!("{summary} — run: catenary doctor"),
        );
        return true;
    };
    if crate::notify::QuarantineStamp::new().should_notify(&config_path) {
        crate::notify::notify_desktop(
            "Catenary command filtering is OFF",
            &format!("{summary} — run: catenary doctor"),
        );
        return true;
    }
    false
}

/// Resolve the quarantine verdict for a foreign shell command, firing the
/// once-per-mtime desktop notification as a side effect (bug 110).
///
/// Returns `None` when `[commands]` loaded cleanly — the caller proceeds with
/// normal enforcement. `Some(verdict)` when the section is quarantined: either
/// fail-open (allow, with an onset-gated `additionalContext` warning) or
/// fail-closed (deny, per the `client_enforcement_only` opt-in). A config that
/// fails to LOAD entirely (document-fatal) is not a quarantine — `None`, and the
/// existing fail-open path in [`check_shell_command`] handles it.
fn commands_quarantine_verdict() -> Option<QuarantineVerdict> {
    let config = crate::config::Config::load().ok()?;
    let section = config.quarantined.section("commands")?;
    let error = section.first_error().to_string();
    let summary = config
        .quarantined
        .summary()
        .unwrap_or_else(|| format!("[commands] quarantined: {error}"));

    let onset = commands_quarantine_onset(&summary);

    if raw_client_enforcement_only() {
        // Fail-closed opt-in: the lever doubles as "deny when broken". Deny on
        // every call, not just the onset.
        Some(QuarantineVerdict::Deny(format!(
            "Catenary command filtering could not load: {error}. \
             `client_enforcement_only = true` is set, so commands are DENIED until the \
             config is fixed. Run `catenary doctor`.",
        )))
    } else {
        // Fail-open: enforcement is off. Tell the agent so, but only on the onset.
        let context = onset.then(|| {
            format!(
                "[commands] quarantined: {error}. Command filtering is OFF until the \
                 config is fixed — catenary doctor",
            )
        });
        Some(QuarantineVerdict::AllowWithContext { context })
    }
}

fn foreign_command_outcome(
    hook_json: &serde_json::Value,
    shell_cmd: &str,
    format: HostFormat,
) -> Result<Vec<PathBuf>, String> {
    match check_shell_command(hook_json, shell_cmd, format) {
        Ok(writes) => Ok(writes.writes.into_iter().collect()),
        Err(boxed) => {
            let (denial, resolved) = *boxed;
            let build_hint =
                resolve_client_build_hint(hook_json, &denial.command, &resolved, format);
            Err(crate::cli::command_filter::format_denial(
                &denial.command,
                &resolved,
                &denial,
                Some(format),
                build_hint.as_deref(),
            ))
        }
    }
}

/// Check a shell command against the configured allowlist.
///
/// Loads user config, then merges with the `cwd`'s project config (if any)
/// for per-root `build` tool support. This is the sole, authoritative
/// enforcement path: enforcement keys are user-level, so the local verdict
/// matches what any daemon round-trip would reach, and the local cwd is more
/// accurate than the daemon's view.
///
/// `Ok(writes)` — the command is allowed and `writes` is its resolved
/// write-set (ws38 ticket 02). `Err((denial, resolved))` — the command is
/// denied, with the resolved config it was judged against.
fn check_shell_command(
    hook_json: &serde_json::Value,
    cmd: &str,
    format: HostFormat,
) -> Result<
    LineWrites,
    Box<(
        crate::cli::command_filter::Denial,
        crate::config::ResolvedCommands,
    )>,
> {
    // A missing / unloadable config is not a denial — allow, with no writes to
    // attribute (the resolver never ran).
    let Ok(config) = crate::config::Config::load() else {
        return Ok(LineWrites::default());
    };
    let Some(resolved) = config.resolved_commands else {
        return Ok(LineWrites::default());
    };
    let cwd = extract_cwd_str(hook_json, format).map(PathBuf::from);
    check_resolved_command(resolved, cmd, cwd)
}

/// The config-independent core of [`check_shell_command`]: given an already
/// loaded user-level [`ResolvedCommands`] and the hook's resolved `cwd`, apply
/// the client-side gating and run the allowlist filter.
///
/// Split out from [`check_shell_command`] so the gating decision is unit
/// testable without touching `Config::load()` (which reads process env and the
/// user's home directory). Behavior is identical to the inlined form:
///
/// - `client_enforcement_only` short-circuits to allow (`Ok`, empty write-set)
///   **before** any project merge — that flag means "don't enforce
///   client-side", so nothing is resolved or attributed.
/// - the `cwd`'s nearest project `.catenary.toml` contributes its per-root
///   `build` tool (enforcement keys stay user-level; see
///   [`merge_project_commands`](crate::config::ResolvedCommands::merge_project_commands)).
/// - an inactive allowlist (`!is_active()`) short-circuits to allow.
/// - otherwise the command faces
///   [`check_and_resolve_command`](crate::cli::command_filter::check_and_resolve_command):
///   `Ok(writes)` on allow, or `Err((denial, resolved))` on deny with the
///   resolved config it was judged against.
#[allow(
    clippy::needless_pass_by_value,
    reason = "owns `resolved`: it is reassigned by the project merge and returned \
              in the denial tuple"
)]
fn check_resolved_command(
    mut resolved: crate::config::ResolvedCommands,
    cmd: &str,
    cwd: Option<PathBuf>,
) -> Result<
    LineWrites,
    Box<(
        crate::cli::command_filter::Denial,
        crate::config::ResolvedCommands,
    )>,
> {
    if resolved.client_enforcement_only {
        return Ok(LineWrites::default());
    }

    // Merge with cwd's project config for per-root build support.
    // Walk up from cwd to find the nearest `.catenary.toml` — cwd is
    // typically a subdirectory of the workspace root.
    // This covers the common single-root case and "agent is in the right
    // directory" case. Multi-root coverage requires the session-side check.
    if let Some(ref cwd_path) = cwd
        && let Some((root, pc)) = find_project_config(cwd_path)
    {
        let mut project_commands = std::collections::HashMap::new();
        if let Some(cmds) = pc.commands {
            project_commands.insert(root.clone(), cmds);
        }
        resolved = resolved.merge_project_commands(std::slice::from_ref(&root), &project_commands);
    }

    if !resolved.is_active() {
        return Ok(LineWrites::default());
    }

    match crate::cli::command_filter::check_and_resolve_command(cmd, &resolved, cwd.as_deref()) {
        Ok(writes) => Ok(writes),
        Err(denial) => Err(Box::new((denial, resolved))),
    }
}

/// Resolve build guidance for the client-side fallback path.
///
/// Uses the hook's `cwd` and user config to construct a [`BuildContext`].
/// Returns `None` when the denied command has no build guidance.
fn resolve_client_build_hint(
    hook_json: &serde_json::Value,
    denied_cmd: &str,
    resolved: &crate::config::ResolvedCommands,
    format: HostFormat,
) -> Option<String> {
    let lookup = denied_cmd.split_whitespace().next().unwrap_or(denied_cmd);
    let crate::config::GuidanceEntry::Build(bg) = resolved.guidance_for(lookup)? else {
        return None;
    };

    let user_config_path = crate::config::config_sources()
        .first()
        .map(|p| p.display().to_string());
    let user_path_str = user_config_path.as_deref().unwrap_or("user config");

    let cwd = extract_cwd_str(hook_json, format).map(PathBuf::from);
    let project = cwd.as_deref().and_then(find_project_config);
    let proj_path = project
        .as_ref()
        .map(|(r, _)| r.join(".catenary.toml").display().to_string());
    let proj_build = project
        .as_ref()
        .and_then(|(_, pc)| pc.commands.as_ref())
        .and_then(|cmds| cmds.build.as_ref())
        .map_or(&[] as &[String], |sv| &sv.0);

    let cwd_str = cwd.as_ref().map(|p| p.display().to_string());
    let ctx = crate::config::BuildContext {
        user_config_path: user_path_str,
        default_build: &resolved.default_build,
        has_project_config: project.is_some(),
        project_config_path: proj_path.as_deref(),
        project_build: proj_build,
        cwd_resolved: cwd.is_some(),
        resolved_cwd_path: cwd_str.as_deref(),
    };

    Some(bg.resolve(&ctx))
}

/// Walk up from `cwd` to find the nearest `.catenary.toml`.
///
/// Stops at the user's home directory — a project config above `$HOME`
/// would be unusual, and walking into `/` is wasteful.
///
/// Returns `(root_path, ProjectConfig)` if found. Errors are silently
/// ignored — a broken project config should not prevent the command
/// filter from running with the user config.
///
/// Shared with `catenary commands` (`cli::commands::run_commands`) so its
/// per-cwd build resolution matches this client-side denial path exactly.
pub(crate) fn find_project_config(
    cwd: &std::path::Path,
) -> Option<(PathBuf, crate::config::ProjectConfig)> {
    let home = dirs::home_dir();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Ok(Some(pc)) = crate::config::load_project_config(d) {
            return Some((d.to_path_buf(), pc));
        }
        // Stop at home directory.
        if home.as_deref() == Some(d) {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Extract the shell command string from hook JSON for Bash-like tools.
///
/// Returns `Some(command)` for the host's shell tool (Claude Code `Bash`,
/// Antigravity `run_command`, OpenCode `bash`). Returns `None` for all other
/// tools.
fn extract_shell_command(
    hook_json: &serde_json::Value,
    tool_name: &str,
    format: HostFormat,
) -> Option<String> {
    let is_shell_tool = match format {
        HostFormat::Claude => tool_name == "Bash",
        HostFormat::Antigravity => tool_name == "run_command",
        // OpenCode's shell tool is `bash`; its command lives in `args.command`,
        // already covered by the `args`/`command` fallbacks below.
        HostFormat::OpenCode => tool_name == "bash",
    };
    if !is_shell_tool {
        return None;
    }
    let tool_input = hook_json
        .get("tool_input")
        .or_else(|| hook_json.get("args"))
        // Antigravity CLI: toolCall.args
        .or_else(|| hook_json.get("toolCall").and_then(|tc| tc.get("args")));
    tool_input
        // Claude Code: "command"; Antigravity CLI: "CommandLine"
        .and_then(|ti| ti.get("command").or_else(|| ti.get("CommandLine")))
        .and_then(|c| c.as_str())
        .map(String::from)
}

// ── Catenary CLI command hooks ──────────────────────────────────────────

/// Handle `PreToolUse` for `catenary editing start`.
///
/// Sends `pre-tool/editing-start` IPC to the daemon to enter editing mode.
/// The daemon checks the editing guardrail. Returns allow (silent) or deny
/// (prints denial reason to stdout for the host CLI).
fn handle_start_editing_hook(hook_json: &serde_json::Value, format: HostFormat) {
    let Some(stream) = hook_connect(hook_json) else {
        return;
    };

    let agent_id = extract_agent_id(hook_json);
    let session_id = extract_session_id(hook_json, format);

    let mut request = serde_json::json!({
        "method": "pre-tool/editing-start",
        "agent_id": agent_id,
        "format": format.as_str(),
    });
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }
    request["host_payload"] = prepare_host_payload(hook_json);

    let lines = ipc_exchange(stream, &request);

    if let Some(line) = lines.first()
        && let Ok(envelope) = serde_json::from_str::<crate::hook::HookResponseEnvelope>(line)
        && let Some(crate::hook::HookResult::Deny(reason)) = &envelope.result
    {
        print!("{}", format_deny(reason, format));
    }
}

/// Handle `PreToolUse` for `catenary diagnostics` (root-ownership stage 3,
/// deliverable 4: diagnostics gating moves to the hook).
///
/// The identity-correlation prepare handoff (`pre-tool/editing-stop`) retired
/// with stage 3 — the daemon now serves diagnoses against the durable ledger, so
/// this hook stages nothing. Its sole job is the OWNER GATE: only the lock holder
/// may pull a locked root's ledger via **bare** `catenary diagnostics`. The hook
/// is the one seam with identity, so it — not the identity-less serve path —
/// answers "is this the owner?". A non-owner is denied naming the owed root and
/// taught `catenary claim <root>`; the owner (or an unlocked root) is allowed
/// silently and the CLI runs `tool/editing-stop`.
///
/// **Scoped** `catenary diagnostics <path…>` names explicit paths and serves them
/// regardless of ownership or debt — the pull-anything arm (a diagnose of a named
/// file is a read, not a payment against someone's kitchen). Only the bare form,
/// which pulls the whole ledger for the cwd's root, is owner-gated.
///
/// No daemon connection is made here — the lock is a filesystem fact
/// ([`crate::lock`]), so the gate works with the daemon down (the serve itself
/// then fails at `tool/editing-stop`, but the gate never false-denies).
fn handle_done_editing_hook(
    hook_json: &serde_json::Value,
    command: Option<&str>,
    format: HostFormat,
) {
    // Scoped iff the command names any path after `diagnostics` — the
    // pull-anything arm, never owner-gated.
    if command.is_some_and(diagnostics_is_scoped) {
        return;
    }

    // Bare form: resolve the cwd's kitchen and gate on ownership. An unresolvable
    // cwd (scratch dir, no VCS checkout) resolves to no root — nothing to gate,
    // allow (the serve then answers `[no edited files]`).
    let cwd = extract_cwd_str(hook_json, format).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let Some(root) = crate::lock::resolve_lock_root(&cwd) else {
        return;
    };

    let owner = crate::lock::Owner::new(
        format.as_str(),
        extract_session_id(hook_json, format).unwrap_or_default(),
        extract_agent_id(hook_json),
    );

    // Only a LOCKED root held by ANOTHER owner is gated. An unlocked root (no
    // lock dir), or one this caller owns, serves freely. `owner_of` reads the
    // owner file by pure path algebra — canonical root, matching the ledger seam.
    if let Some(holder) = crate::lock::owner_of(&root)
        && holder != owner
    {
        print!("{}", format_deny(&diagnostics_locked_deny(&root), format));
    }
}

/// Whether a `catenary diagnostics` command line names any path argument (the
/// scoped, pull-anything form) rather than running bare.
///
/// Splits on ASCII whitespace, advances to the `diagnostics` word, and reports
/// whether any following non-flag token is present. Mirrors [`extract_claim_root`]
/// for the sibling command.
fn diagnostics_is_scoped(command: &str) -> bool {
    let mut toks = command.split_whitespace();
    for tok in toks.by_ref() {
        if tok == "diagnostics" {
            break;
        }
    }
    toks.any(|t| !t.starts_with('-'))
}

/// The deny briefing shown when a non-owner runs bare `catenary diagnostics`
/// against a root another agent holds (root-ownership stage 3, deliverable 4).
///
/// Names the owed root on its own copy-pasteable line and teaches the takeover
/// path: pulling another editor's ledger would serve work the reader did not
/// author, so the gate points at `catenary claim <root>` (which transfers the
/// root and its debt) or the scoped form (which serves named paths regardless).
fn diagnostics_locked_deny(root: &std::path::Path) -> String {
    let root = root.display();
    format!(
        "root locked: {root}\n\
         `catenary diagnostics` (bare) pulls this root's edit ledger, but another agent holds it — \
         its debt is theirs to diagnose, not yours to read.\n\
         To take over the root and its diagnostic debt:\n\
         \x20 catenary claim {root}\n\
         Or diagnose specific files regardless of ownership:\n\
         \x20 catenary diagnostics <path…>"
    )
}

/// Extract the root-path argument from a `catenary claim <root>` command line.
///
/// The command reached here as a bare, canonical `catenary claim …` (the
/// isolation gate guarantees the sole command), so the token after `claim` is
/// the root. Splits on ASCII whitespace and returns the first non-flag token
/// following `claim`. Returns `None` when no argument was supplied (the CLI then
/// prints the clap usage error).
fn extract_claim_root(command: &str) -> Option<String> {
    let mut toks = command.split_whitespace();
    // Advance to the `claim` word.
    for tok in toks.by_ref() {
        if tok == "claim" {
            break;
        }
    }
    // The first following non-flag token is the root.
    toks.find(|t| !t.starts_with('-')).map(str::to_string)
}

/// Handle `PreToolUse` for `catenary claim <root>` (root-ownership stage 2).
///
/// The identity tuple lives at THIS seam. The hook forwards the claimant's
/// identity (`format`+`session`+`agent`) and the resolved root to the daemon via
/// `pre-tool/claim`, which runs the mechanical guard (refuse while a diagnose
/// round is in flight), performs the one atomic owner-file rename, records the
/// takeover (firehose + warn finding), and stages the rendered answer for the
/// CLI to print. The hook then ALLOWS the command (prints nothing) so the CLI
/// runs and drains the answer.
///
/// Degrade-open when the daemon is unreachable: the lock is a hook-plane fact, so
/// the hook performs the rename itself and allows — the CLI's own degrade path
/// reads the post-rename lock state to print a confirmation. A guard refusal from
/// the daemon (a diagnose round in flight) is surfaced to the agent as a deny.
fn handle_claim_hook(hook_json: &serde_json::Value, command: &str, format: HostFormat) {
    let Some(root_arg) = extract_claim_root(command) else {
        // No root supplied — let the CLI's clap layer print the usage error.
        return;
    };
    // Resolve to an absolute, canonical path so the encoding matches the
    // acquisition-time root.
    let root = std::path::Path::new(&root_arg);
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        extract_cwd_str(hook_json, format)
            .map_or_else(
                || std::env::current_dir().unwrap_or_default(),
                PathBuf::from,
            )
            .join(root)
    };
    let root = root.canonicalize().unwrap_or(root);

    let owner = crate::lock::Owner::new(
        format.as_str(),
        extract_session_id(hook_json, format).unwrap_or_default(),
        extract_agent_id(hook_json),
    );

    let Some(stream) = hook_connect(hook_json) else {
        // Daemon down — degrade open: perform the rename hook-local (the lock is
        // a filesystem fact). No firehose/warn (those need the daemon); the CLI
        // reads the resulting lock state to print its own confirmation.
        let _ = crate::lock::claim(&root, &owner, std::time::SystemTime::now());
        return;
    };

    let request = serde_json::json!({
        "method": "pre-tool/claim",
        "format": format.as_str(),
        "session_id": owner.session,
        "agent_id": owner.agent,
        "root": root.display().to_string(),
    });
    let lines = ipc_exchange(stream, &request);

    // A guard refusal (a diagnose round in flight) is surfaced to the agent as a
    // deny; every other outcome allows (the CLI prints the staged answer, or its
    // own degrade confirmation).
    if let Some(line) = lines.first()
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
        && v.get("status").and_then(serde_json::Value::as_str) == Some("refused")
    {
        let reason = v
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("claim refused: a diagnose round is in flight")
            .to_string();
        print!("{}", format_deny(&reason, format));
    }
}

// ── The reconcile bracket (root-ownership stage 5) ──────────────────────────

/// The `PostToolUse` hook handler — the reconcile bracket's post-command leg
/// (root-ownership stage 5).
///
/// A stateful-tier git command that moves the working tree
/// (`stash`/`checkout`/`stash pop`/`merge`/`rebase`) gets reconciled against git
/// itself as the changed-ness oracle (`git status --porcelain`), both directions:
/// `stash`/`checkout` unbook files git now reports clean, `stash pop`/`merge`/
/// `rebase` book every covered file git reports modified ("the pop should present
/// as writes that restore it"). **Attribution is clean** because the bracket
/// wraps the cook's OWN command at THIS seam — no watcher guesswork.
///
/// Runs entirely hook-process-local (the ledger is a filesystem fact) and
/// **returns no decision**: `PostToolUse` carries none, so this never interferes
/// with the host's flow. Every reconcile leg (unparsable stdin, a non-git or
/// non-reconciling command, an unresolvable root, a failed git query) is a silent
/// no-op. Only Claude and OpenCode carry a `Bash` shell tool through
/// `PostToolUse`; other hosts reconcile nothing (their shell-tool extractor yields
/// `None`).
///
/// The reserved-shim dialect contract is preserved (this event stays wired across
/// the full surface): Claude tolerates silence, but Antigravity's contract is
/// JSON-in/JSON-out, so an Antigravity host always gets the documented empty
/// object `{}` — the reconcile is the added behavior, the empty answer the
/// unchanged floor.
pub fn run_post_tool(format: HostFormat) {
    // Drain-and-parse; on any failure still answer the dialect's empty form so a
    // garbage payload never breaks the host's flow (the shim contract).
    if let Ok(stdin_data) = std::io::read_to_string(std::io::stdin())
        && let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data)
    {
        let tool_name = extract_tool_name(&hook_json, format);
        if let Some(command) = extract_shell_command(&hook_json, tool_name, format) {
            reconcile_stateful_git(&hook_json, &command, format);
        }

        // The secret-redaction backstop (misc 201, component 3). Scans EVERY
        // tool's output (Read and Bash both matter; PostToolUse fires for
        // subagents too and receives the COMPLETE output) for high-confidence
        // secret shapes and, on a hit, emits `updatedToolOutput` with the spans
        // replaced by a marker naming what was redacted. A clean output emits
        // nothing — the original bytes pass through byte-identical. Claude Code
        // only; the shape is Claude's, and no other host carries it.
        if matches!(format, HostFormat::Claude) && redact_tool_output(&hook_json) {
            return;
        }
    }
    // Antigravity's JSON-in/JSON-out contract: answer the empty object. Claude /
    // OpenCode tolerate silence, so nothing is emitted for them.
    if matches!(format, HostFormat::Antigravity) {
        print!("{{}}");
    }
}

/// Scan the `PostToolUse` tool output for high-confidence secret shapes and, on a
/// hit, print the `hookSpecificOutput.updatedToolOutput` redaction response
/// (misc 201, component 3). Returns `true` iff something was emitted.
///
/// This is the ONE emission site for the redaction wire shape, so a field rename
/// is a one-place fix. The scanned output is Claude Code's `PostToolUse`
/// `tool_response` — a bare string, or an object/array whose string leaves are
/// scanned and reassembled. A clean output emits nothing and returns `false`, so
/// the untouched output passes through byte-identical (no `updatedToolOutput`).
fn redact_tool_output(hook_json: &serde_json::Value) -> bool {
    let Some(response) = hook_json.get("tool_response") else {
        return false;
    };
    // Only the string-valued output can carry a scannable secret. A structured
    // `tool_response` (Read returns `{file: {…}}`) is rendered to its JSON text
    // for the scan; on a hit we replace the whole field with the redacted STRING
    // form (the marker survives, the rest is preserved) — never dropping output.
    let text = match response {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let Some(redacted) = crate::answer_desk::redact_secrets(&text) else {
        return false;
    };
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "updatedToolOutput": redacted,
        }
    });
    print!("{out}");
    true
}

/// Run the reconcile bracket for a stateful-tier git command, if the command
/// drives a reconcile (root-ownership stage 5).
///
/// Classifies the command's reconcile direction
/// ([`tier::git_reconcile_direction`](crate::cli::command_filter::tier::git_reconcile_direction)):
/// a non-reconciling command (`git commit`, a read-only git, a non-git command)
/// is a no-op. Otherwise resolves the command's root from cwd, runs the
/// `git status --porcelain` oracle in that root, and drives
/// [`crate::lock::reconcile_bracket`] in the classified direction under the cook's
/// own identity. All paths (the oracle's, the ledger's) canonicalize at their
/// ingestion seam so the reconcile keys the SAME canonical ledger the edit seam
/// booked under (the spelling rule).
fn reconcile_stateful_git(hook_json: &serde_json::Value, command: &str, format: HostFormat) {
    use crate::cli::command_filter::tier;

    let Some(direction) = tier::git_reconcile_direction(command) else {
        return; // not a reconciling git command (commit, read-only, non-git)
    };

    // The command's kitchen: resolve the cwd to its lock root. An unresolvable
    // cwd (scratch dir, no VCS checkout) has no ledger to reconcile.
    let cwd = extract_cwd_str(hook_json, format).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let Some(root) = crate::lock::resolve_lock_root(&cwd) else {
        return;
    };

    // Book direction needs the static booking data (a pop never books an
    // uncoverable file — the same gate the edit seam uses). A config that won't
    // load fails open: skip the reconcile rather than mis-book.
    let Ok(config) = crate::config::Config::load() else {
        return;
    };
    let booking = crate::lock::Booking::from_config(&config);

    let owner = crate::lock::Owner::new(
        format.as_str(),
        extract_session_id(hook_json, format).unwrap_or_default(),
        extract_agent_id(hook_json),
    );

    // The oracle: git's own changed-ness report, canonicalized against the repo
    // root. A failed query yields an empty set — in the Unbook direction that
    // would clear all debt (wrong), so a failed query is a no-op instead.
    let Some(modified) = git_status_modified(&cwd) else {
        return;
    };

    crate::lock::reconcile_bracket(&root, &owner, &booking, direction, &modified);
}

/// Run `git status --porcelain` in `cwd` and return the canonical absolute paths
/// git reports as changed — the reconcile bracket's changed-ness oracle
/// (root-ownership stage 5).
///
/// Non-interactive and read-only (`GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`,
/// stdin closed), mirroring the write resolver's git-query discipline. The porcelain
/// paths are **repo-relative**, so they join onto the repository root
/// (`git rev-parse --show-toplevel`) — never the cwd — then canonicalize at the
/// ingestion seam (the spelling rule) so they key the same canonical ledger the
/// edit seam booked under. Returns `None` on any failure (git missing, not a repo,
/// non-zero exit, non-UTF-8) so the caller no-ops rather than reconcile against a
/// phantom empty set.
fn git_status_modified(cwd: &std::path::Path) -> Option<Vec<PathBuf>> {
    let repo_root = git_query(cwd, &["rev-parse", "--show-toplevel"])?
        .into_iter()
        .next()
        .map(PathBuf::from)?;
    let lines = git_query(cwd, &["status", "--porcelain", "--no-renames"])?;
    let paths = lines
        .iter()
        .filter_map(|line| porcelain_path(line))
        .map(|rel| {
            let joined = repo_root.join(rel);
            joined.canonicalize().unwrap_or(joined)
        })
        .collect();
    Some(paths)
}

/// Parse the repo-relative path out of one `git status --porcelain` line.
///
/// Porcelain v1 lines are `XY <path>` — two status columns, a space, then the
/// path (rename arrows are suppressed by `--no-renames`, so no ` -> ` split is
/// needed). The path starts at byte offset 3. A short/malformed line yields
/// `None`. A quoted path (git quotes paths with unusual bytes when `core.quotePath`
/// is on) is left as-is: it won't match a booked touch leaf and simply reconciles
/// nothing for that entry — the safe direction, never a mis-book.
fn porcelain_path(line: &str) -> Option<&str> {
    let path = line.get(3..)?.trim();
    (!path.is_empty()).then_some(path)
}

/// Run a non-interactive, read-only git query in `cwd`, returning its stdout
/// lines on a clean (exit 0) run, or `None` on any failure.
///
/// The reconcile bracket's private twin of the write resolver's git query
/// (`resolver::git::git_query`, not re-exported): same discipline
/// (`GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`, stdin closed), so a
/// bracket query is as inert and hermetic as a resolver query.
fn git_query(cwd: &std::path::Path, args: &[&str]) -> Option<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.lines().map(str::to_string).collect())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};

    #[test]
    fn diagnostics_is_scoped_distinguishes_bare_from_scoped() {
        // Bare — no path argument follows `diagnostics`.
        assert!(!diagnostics_is_scoped("catenary diagnostics"));
        assert!(!diagnostics_is_scoped(
            "/usr/local/bin/catenary diagnostics"
        ));
        // Scoped — a path (or a dir, or `.`) follows.
        assert!(diagnostics_is_scoped("catenary diagnostics src/main.rs"));
        assert!(diagnostics_is_scoped("catenary diagnostics ."));
        assert!(diagnostics_is_scoped("catenary diagnostics src/ lib.rs"));
        // A flag alone is still bare (the owner gate applies).
        assert!(!diagnostics_is_scoped("catenary diagnostics --help"));
    }

    #[test]
    fn diagnostics_locked_deny_names_root_and_teaches() {
        let root = std::path::Path::new("/home/mark/Projects/Catenary");
        let msg = diagnostics_locked_deny(root);
        assert!(msg.starts_with("root locked: /home/mark/Projects/Catenary\n"));
        assert!(msg.contains("another agent holds it"));
        assert!(msg.contains("catenary claim /home/mark/Projects/Catenary"));
        assert!(msg.contains("catenary diagnostics <path…>"));
    }

    #[test]
    fn extract_claim_root_reads_the_root_argument() {
        assert_eq!(
            extract_claim_root("catenary claim /home/mark/Projects/Catenary"),
            Some("/home/mark/Projects/Catenary".to_string()),
        );
        // A flag before the root is skipped; the first non-flag token wins.
        assert_eq!(
            extract_claim_root("catenary claim --force /repo"),
            Some("/repo".to_string()),
        );
        // A leading path to the binary does not confuse the `claim` anchor.
        assert_eq!(
            extract_claim_root("/usr/local/bin/catenary claim /repo"),
            Some("/repo".to_string()),
        );
        // No argument → None (the CLI's clap layer reports the usage error).
        assert!(extract_claim_root("catenary claim").is_none());
    }

    // ── Reconcile bracket: porcelain parsing (root-ownership stage 5) ────

    #[test]
    fn porcelain_path_extracts_the_relative_path() {
        // `XY <path>` — two status columns, a space, then the repo-relative path.
        assert_eq!(porcelain_path(" M src/main.rs"), Some("src/main.rs"));
        assert_eq!(porcelain_path("M  src/main.rs"), Some("src/main.rs"));
        assert_eq!(porcelain_path("?? new.rs"), Some("new.rs"));
        assert_eq!(porcelain_path("A  src/added.rs"), Some("src/added.rs"));
    }

    #[test]
    fn porcelain_path_rejects_short_or_empty_lines() {
        assert!(porcelain_path("").is_none());
        assert!(porcelain_path(" M ").is_none());
        assert!(porcelain_path("XY").is_none());
    }

    #[test]
    fn additional_context_claude_shape() -> Result<()> {
        // The parent-agent notice rides `hookSpecificOutput.additionalContext`
        // for the given hook event (misc 151).
        let output = format_additional_context(
            "subagent `a` left a dirty worktree",
            "PreToolUse",
            HostFormat::Claude,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("PreToolUse"),
        );
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"].as_str(),
            Some("subagent `a` left a dirty worktree"),
        );
        Ok(())
    }

    #[test]
    fn additional_context_non_claude_is_empty() {
        // Other hosts have no `additionalContext` field; nothing is emitted.
        assert!(format_additional_context("x", "Stop", HostFormat::Antigravity).is_empty());
        assert!(format_additional_context("x", "Stop", HostFormat::OpenCode).is_empty());
    }

    // ── Reserved-shim drain tests ───────────────────────────────────

    #[test]
    fn drain_hook_stdin_consumes_well_formed_json_to_eof() {
        // The shim's whole job: read the host's payload to EOF (no host-side
        // EPIPE) and drop it. The byte count proves full consumption.
        let payload = br#"{"session_id":"s1","hook_event_name":"PostToolUse","cwd":"/tmp"}"#;
        assert_eq!(
            drain_hook_stdin(std::io::Cursor::new(&payload[..])),
            payload.len() as u64,
        );
    }

    #[test]
    fn drain_hook_stdin_consumes_garbage_and_empty_input() {
        // Malformed JSON, invalid UTF-8, and empty stdin all drain cleanly —
        // the shim succeeds regardless of payload content.
        let garbage: &[u8] = &[0xFF, 0xFE, b'{', 0x00, b'x'];
        assert_eq!(drain_hook_stdin(std::io::Cursor::new(garbage)), 5);
        assert_eq!(
            drain_hook_stdin(std::io::Cursor::new(&b"not json at all"[..])),
            15,
        );
        assert_eq!(drain_hook_stdin(std::io::Cursor::new(&b""[..])), 0);
    }

    // ── extract_shell_command tests ─────────────────────────────────

    #[test]
    fn extract_shell_command_claude_bash() {
        let json = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "ls -la" }
        });
        assert_eq!(
            extract_shell_command(&json, "Bash", HostFormat::Claude),
            Some("ls -la".to_string()),
        );
    }

    #[test]
    fn extract_shell_command_non_bash_returns_none() {
        let json = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "src/main.rs" }
        });
        assert!(extract_shell_command(&json, "Edit", HostFormat::Claude).is_none());
    }

    #[test]
    fn extract_shell_command_wrong_format_returns_none() {
        // Bash tool name with Antigravity format → not a shell tool
        let json = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "ls" }
        });
        assert!(extract_shell_command(&json, "Bash", HostFormat::Antigravity).is_none());
    }

    #[test]
    fn extract_shell_command_missing_command_field() {
        let json = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {}
        });
        assert!(extract_shell_command(&json, "Bash", HostFormat::Claude).is_none());
    }

    // ── Host payload truncation tests ─────────────────────────────────

    #[test]
    fn truncate_preserves_short_strings() {
        let mut val = serde_json::json!({
            "cwd": "/home/user/project",
            "session_id": "abc-123",
            "hook_event_name": "PreToolUse"
        });
        truncate_host_strings(&mut val, 512);
        assert_eq!(val["cwd"], "/home/user/project");
        assert_eq!(val["session_id"], "abc-123");
        assert_eq!(val["hook_event_name"], "PreToolUse");
    }

    #[test]
    fn truncate_at_newline() {
        let mut val = serde_json::json!({
            "tool_input": {
                "file_path": "/src/main.rs",
                "old_string": "fn main() {\n    println!(\"hello\");\n}\n",
                "new_string": "fn main() {\n    eprintln!(\"hello\");\n}\n"
            }
        });
        truncate_host_strings(&mut val, 512);
        assert_eq!(val["tool_input"]["file_path"], "/src/main.rs");
        assert_eq!(val["tool_input"]["old_string"], "fn main() {…");
        assert_eq!(val["tool_input"]["new_string"], "fn main() {…");
    }

    #[test]
    fn truncate_at_max_chars_without_newline() {
        let long = "a".repeat(600);
        let mut val = serde_json::json!({"field": long});
        truncate_host_strings(&mut val, 512);
        let result = val["field"].as_str().expect("string");
        // 512 'a' chars + '…'
        assert_eq!(result.chars().count(), 513);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // 600 multi-byte characters — should truncate at the 512th char,
        // not at byte 512 (which would split a character).
        let long: String = std::iter::repeat_n('é', 600).collect();
        let mut val = serde_json::json!({"field": long});
        truncate_host_strings(&mut val, 512);
        let result = val["field"].as_str().expect("string");
        assert_eq!(result.chars().count(), 513); // 512 'é' + '…'
        assert!(result.ends_with('…'));
        assert!(result.starts_with('é'));
    }

    #[test]
    fn truncate_newline_before_max_chars_wins() {
        // Newline at character 10, max_chars 512 → truncate at 10.
        let mut val = serde_json::json!({"field": "short line\nrest of content"});
        truncate_host_strings(&mut val, 512);
        assert_eq!(val["field"], "short line…");
    }

    #[test]
    fn truncate_recurses_into_arrays() {
        let mut val = serde_json::json!({
            "items": ["ok", "multi\nline", "also\nhere"]
        });
        truncate_host_strings(&mut val, 512);
        assert_eq!(val["items"][0], "ok");
        assert_eq!(val["items"][1], "multi…");
        assert_eq!(val["items"][2], "also…");
    }

    #[test]
    fn truncate_leaves_non_strings_unchanged() {
        let mut val = serde_json::json!({
            "count": 42,
            "active": true,
            "data": null
        });
        truncate_host_strings(&mut val, 512);
        assert_eq!(val["count"], 42);
        assert_eq!(val["active"], true);
        assert!(val["data"].is_null());
    }

    #[test]
    fn prepare_host_payload_truncates() {
        let hook_json = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_input": {
                "file_path": "/src/lib.rs",
                "old_string": "line1\nline2\nline3\nline4"
            }
        });
        let prepared = prepare_host_payload(&hook_json);
        assert_eq!(prepared["cwd"], "/home/user/project");
        assert_eq!(prepared["tool_input"]["file_path"], "/src/lib.rs");
        assert_eq!(prepared["tool_input"]["old_string"], "line1…");
    }

    // ── format_deny tests ────────────────────────────────────────────

    #[test]
    fn format_deny_claude_structure() -> Result<()> {
        let output = format_deny("command not allowed", HostFormat::Claude);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny",);
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"],
            "command not allowed",
        );
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse",);
        Ok(())
    }

    // ── check_resolved_command (client-side allowlist gate) tests ─────
    //
    // These pin the *decision* the client-side fallback makes — the gap the
    // `check_shell_command -> None` mutant flagged. `check_shell_command` itself
    // is a thin `Config::load()` + delegate wrapper that can't be exercised
    // in-process (env/home coupling; Rust 2024 `set_var` is `unsafe`, which is
    // `forbid`den here), so the testable gating lives in `check_resolved_command`
    // and is verified directly. `cwd: None` keeps the run hermetic — no project
    // `.catenary.toml` walk.

    /// Build a user-level `ResolvedCommands` from inline TOML (no env, no
    /// filesystem walk), mirroring how the hook's `Config::load()` would resolve
    /// the `[commands]` table.
    fn resolved_from_toml(toml: &str) -> crate::config::ResolvedCommands {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).expect("write config");
        crate::config::Config::load_from_sources(&[path])
            .expect("config should parse")
            .resolved_commands
            .expect("`[commands]` table should resolve")
    }

    #[test]
    fn check_resolved_command_denies_command_off_allowlist() {
        // A command not on the allowlist is denied client-side: the gate returns
        // the actual `Denial` (with the offending command + `NotAllowed`
        // reason), not `None`. This is the assertion the `-> None` mutant
        // (allow-everything) must fail.
        let resolved = resolved_from_toml("[commands]\nallow = [\"git\", \"ls\"]\n");
        let (denial, _) = *check_resolved_command(resolved, "cargo build", None)
            .expect_err("a command off the allowlist must be denied client-side");
        assert_eq!(denial.command, "cargo");
        assert_eq!(
            denial.reason,
            crate::cli::command_filter::DenialReason::NotAllowed,
        );
    }

    #[test]
    fn check_resolved_command_allows_allowlisted_command() {
        // An allowlisted command passes the gate → `Ok` (no denial).
        let resolved = resolved_from_toml("[commands]\nallow = [\"git\", \"ls\"]\n");
        assert!(
            check_resolved_command(resolved, "git status", None).is_ok(),
            "an allowlisted command must pass the client-side gate",
        );
    }

    #[test]
    fn check_resolved_command_client_enforcement_only_allows_all() {
        // `client_enforcement_only = true` means the daemon enforces, not the
        // client fallback — so even an off-allowlist command short-circuits to
        // allow (`Ok`) here. (Built directly: TOML rejects
        // `client_enforcement_only` alongside `allow`, see config validation.)
        let resolved = crate::config::ResolvedCommands {
            client_enforcement_only: true,
            allow: std::collections::HashSet::from(["git".to_string()]),
            ..Default::default()
        };
        assert!(
            check_resolved_command(resolved, "cargo build", None).is_ok(),
            "client_enforcement_only must bypass the client-side gate",
        );
    }

    #[test]
    fn check_resolved_command_inactive_allowlist_allows_all() {
        // An inactive allowlist (no allow/pipeline/build entries) is not
        // enforced client-side, so every command is allowed (`Ok`).
        let resolved = crate::config::ResolvedCommands::default();
        assert!(
            !resolved.is_active(),
            "an empty allowlist should be inactive",
        );
        assert!(
            check_resolved_command(resolved, "cargo build", None).is_ok(),
            "an inactive allowlist must not deny client-side",
        );
    }

    // ── format_stop_block tests ──────────────────────────────────────

    #[test]
    fn format_stop_block_claude_structure() -> Result<()> {
        let output = format_stop_block("files still in editing state", HostFormat::Claude);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["decision"], "block");
        assert_eq!(parsed["reason"], "files still in editing state");
        Ok(())
    }

    // ── extract_file_path tests ──────────────────────────────────────

    #[test]
    fn extract_file_path_absolute() {
        let json = serde_json::json!({
            "tool_input": { "file_path": "/home/user/project/src/main.rs" }
        });
        assert_eq!(
            extract_file_path(&json, HostFormat::Claude),
            Some("/home/user/project/src/main.rs".to_string()),
        );
    }

    #[test]
    fn extract_file_path_file_key() {
        let json = serde_json::json!({
            "tool_input": { "file": "/tmp/test.py" }
        });
        assert_eq!(
            extract_file_path(&json, HostFormat::Claude),
            Some("/tmp/test.py".to_string()),
        );
    }

    #[test]
    fn extract_file_path_relative_with_cwd() {
        let json = serde_json::json!({
            "cwd": "/home/user/project",
            "tool_input": { "file_path": "src/main.rs" }
        });
        let result =
            extract_file_path(&json, HostFormat::Claude).expect("should resolve relative path");
        assert_eq!(result, "/home/user/project/src/main.rs");
    }

    #[test]
    fn extract_file_path_missing_tool_input() {
        let json = serde_json::json!({});
        assert!(extract_file_path(&json, HostFormat::Claude).is_none());
    }

    #[test]
    fn extract_file_path_missing_file_keys() {
        let json = serde_json::json!({
            "tool_input": { "command": "ls" }
        });
        assert!(extract_file_path(&json, HostFormat::Claude).is_none());
    }

    #[test]
    fn extract_file_path_prefers_file_path_over_file() {
        let json = serde_json::json!({
            "tool_input": {
                "file_path": "/preferred/path.rs",
                "file": "/fallback/path.rs"
            }
        });
        assert_eq!(
            extract_file_path(&json, HostFormat::Claude),
            Some("/preferred/path.rs".to_string()),
        );
    }

    // ── ipc_exchange tests ───────────────────────────────────────────

    /// Mock stream for testing `ipc_exchange` without real IPC.
    struct MockStream {
        read_buf: std::io::Cursor<Vec<u8>>,
        write_buf: Vec<u8>,
    }

    impl MockStream {
        fn new(response: &[u8]) -> Self {
            Self {
                read_buf: std::io::Cursor::new(response.to_vec()),
                write_buf: Vec::new(),
            }
        }
    }

    impl std::io::Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.read_buf.read(buf)
        }
    }

    impl std::io::Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.write_buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn ipc_exchange_reads_response_lines() {
        let stream = MockStream::new(b"first response\nsecond response\n");
        let request = serde_json::json!({"method": "test"});
        let lines = ipc_exchange(stream, &request);
        assert_eq!(lines, vec!["first response", "second response"]);
    }

    #[test]
    fn ipc_exchange_stops_at_empty_line() {
        let stream = MockStream::new(b"data line\n\nignored\n");
        let request = serde_json::json!({"method": "test"});
        let lines = ipc_exchange(stream, &request);
        assert_eq!(lines, vec!["data line"]);
    }

    #[test]
    fn ipc_exchange_empty_response_returns_empty_vec() {
        let stream = MockStream::new(b"");
        let request = serde_json::json!({"method": "test"});
        let lines = ipc_exchange(stream, &request);
        assert!(lines.is_empty());
    }

    #[test]
    fn ipc_exchange_writes_json_with_trailing_newline() {
        let mut stream = MockStream::new(b"");
        let request = serde_json::json!({"method": "test-req", "id": 1});
        // Need to keep a reference to check writes, so use a raw pointer trick.
        // Actually, let's just verify it doesn't panic — the write correctness
        // is implicitly tested by the read tests (server receives valid JSON).
        let _ = ipc_exchange(&mut stream, &request);
        let written = String::from_utf8_lossy(&stream.write_buf);
        assert!(
            written.contains("test-req"),
            "should write the request JSON"
        );
        assert!(written.ends_with('\n'), "should end with newline delimiter");
    }

    // ── find_project_config tests ────────────────────────────────────

    #[test]
    fn find_project_config_walks_up_to_parent() {
        let root = tempfile::tempdir().expect("tempdir");
        let subdir = root.path().join("src").join("nested");
        std::fs::create_dir_all(&subdir).expect("create subdirs");
        std::fs::write(
            root.path().join(".catenary.toml"),
            "[lsp.language.rust]\nservers = [\"rust-analyzer\"]\n",
        )
        .expect("write config");

        let result = find_project_config(&subdir);
        let (found_root, config) = result.expect("should find config at parent");
        assert_eq!(found_root, root.path());
        assert!(
            config.language.contains_key("rust"),
            "should parse language config",
        );
    }

    #[test]
    fn find_project_config_returns_none_when_absent() {
        let root = tempfile::tempdir().expect("tempdir");
        let subdir = root.path().join("deep").join("dir");
        std::fs::create_dir_all(&subdir).expect("create subdirs");
        // No .catenary.toml anywhere in the tree.
        assert!(find_project_config(&subdir).is_none());
    }

    #[test]
    fn find_project_config_finds_at_cwd() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join(".catenary.toml"),
            "[lsp.language.python]\nservers = [\"pyright\"]\n",
        )
        .expect("write config");

        let result = find_project_config(root.path());
        let (found_root, _config) = result.expect("should find config at cwd");
        assert_eq!(found_root, root.path());
    }

    // ── notify_connect tests ─────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn notify_connect_returns_stream_for_valid_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("test.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind listener");

        let stream = notify_connect(&sock_path);
        assert!(stream.is_some(), "should connect to a valid socket");
    }

    #[cfg(unix)]
    #[test]
    fn notify_connect_returns_none_for_missing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("nonexistent.sock");
        assert!(notify_connect(&sock_path).is_none());
    }

    // ── Antigravity extraction tests ──────────────────────────────────

    #[test]
    fn antigravity_pre_tool_extracts_session_id() {
        let json = serde_json::json!({
            "conversationId": "conv-abc-123",
            "workspacePaths": ["/home/user/project"],
            "toolCall": {
                "name": "write_to_file",
                "args": { "TargetFile": "/src/main.rs" }
            }
        });
        assert_eq!(
            extract_session_id(&json, HostFormat::Antigravity),
            Some("conv-abc-123"),
        );
    }

    #[test]
    fn antigravity_pre_tool_extracts_file_path() {
        let json = serde_json::json!({
            "conversationId": "conv-1",
            "workspacePaths": ["/home/user/project"],
            "toolCall": {
                "name": "write_to_file",
                "args": { "TargetFile": "/src/main.rs" }
            }
        });
        assert_eq!(
            extract_file_path(&json, HostFormat::Antigravity),
            Some("/src/main.rs".to_string()),
        );
    }

    #[test]
    fn antigravity_pre_tool_extracts_relative_file_path() {
        let json = serde_json::json!({
            "conversationId": "conv-1",
            "workspacePaths": ["/home/user/project"],
            "toolCall": {
                "name": "write_to_file",
                "args": { "TargetFile": "src/main.rs" }
            }
        });
        assert_eq!(
            extract_file_path(&json, HostFormat::Antigravity),
            Some("/home/user/project/src/main.rs".to_string()),
        );
    }

    #[test]
    fn antigravity_deny_response_format() -> Result<()> {
        let output = format_deny(
            "run `catenary editing start` before editing",
            HostFormat::Antigravity,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["decision"], "deny");
        assert_eq!(
            parsed["reason"],
            "run `catenary editing start` before editing",
        );
        // Antigravity format should NOT have hookSpecificOutput
        assert!(parsed["hookSpecificOutput"].is_null());
        Ok(())
    }

    #[test]
    fn antigravity_allow_response_format() {
        // Allow is indicated by printing nothing — no output means allow.
        // The deny format is tested above; absence of output = allow.
        // This test verifies that the deny structure is distinct from allow.
        let output = format_deny("denied", HostFormat::Antigravity);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(parsed["decision"], "deny");
    }

    #[test]
    fn antigravity_command_recognition() {
        let json = serde_json::json!({
            "conversationId": "conv-1",
            "workspacePaths": ["/home/user/project"],
            "toolCall": {
                "name": "run_command",
                "args": { "CommandLine": "catenary editing start" }
            }
        });
        let tool_name = extract_tool_name(&json, HostFormat::Antigravity);
        assert_eq!(tool_name, "run_command");
        assert_eq!(
            extract_shell_command(&json, tool_name, HostFormat::Antigravity),
            Some("catenary editing start".to_string()),
        );
    }

    #[test]
    fn antigravity_stop_block_format() -> Result<()> {
        let output = format_stop_block("files still in editing state", HostFormat::Antigravity);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["decision"], "continue");
        assert_eq!(parsed["reason"], "files still in editing state");
        Ok(())
    }

    // ── extract_session_id tests ──────────────────────────────────────

    #[test]
    fn extract_session_id_claude() {
        let json = serde_json::json!({ "session_id": "claude-sess-1" });
        assert_eq!(
            extract_session_id(&json, HostFormat::Claude),
            Some("claude-sess-1"),
        );
    }

    #[test]
    fn extract_session_id_antigravity() {
        let json = serde_json::json!({ "conversationId": "ag-conv-1" });
        assert_eq!(
            extract_session_id(&json, HostFormat::Antigravity),
            Some("ag-conv-1"),
        );
    }

    #[test]
    fn extract_session_id_missing() {
        let json = serde_json::json!({});
        assert!(extract_session_id(&json, HostFormat::Claude).is_none());
        assert!(extract_session_id(&json, HostFormat::Antigravity).is_none());
    }

    // ── extract_cwd_str tests ─────────────────────────────────────────

    #[test]
    fn extract_cwd_str_claude() {
        let json = serde_json::json!({ "cwd": "/home/user/project" });
        assert_eq!(
            extract_cwd_str(&json, HostFormat::Claude),
            Some("/home/user/project"),
        );
    }

    #[test]
    fn extract_cwd_str_antigravity() {
        let json = serde_json::json!({ "workspacePaths": ["/home/user/project", "/other"] });
        assert_eq!(
            extract_cwd_str(&json, HostFormat::Antigravity),
            Some("/home/user/project"),
        );
    }

    #[test]
    fn extract_cwd_str_missing() {
        let json = serde_json::json!({});
        assert!(extract_cwd_str(&json, HostFormat::Claude).is_none());
        assert!(extract_cwd_str(&json, HostFormat::Antigravity).is_none());
    }

    // ── extract_worktree_path tests ───────────────────────────────────

    #[test]
    fn extract_worktree_path_absolute() {
        let json = serde_json::json!({
            "session_id": "sess-1",
            "worktree_path": "/home/user/.claude/worktrees/agent-1",
        });
        assert_eq!(
            extract_worktree_path(&json, HostFormat::Claude),
            Some("/home/user/.claude/worktrees/agent-1".to_string()),
        );
    }

    #[test]
    fn extract_worktree_path_relative_resolves_against_cwd() {
        let json = serde_json::json!({
            "session_id": "sess-1",
            "cwd": "/home/user/project",
            "worktree_path": ".claude/worktrees/agent-1",
        });
        assert_eq!(
            extract_worktree_path(&json, HostFormat::Claude),
            Some("/home/user/project/.claude/worktrees/agent-1".to_string()),
        );
    }

    #[test]
    fn extract_worktree_path_missing() {
        let json = serde_json::json!({ "session_id": "sess-1" });
        assert!(extract_worktree_path(&json, HostFormat::Claude).is_none());
    }

    // ── extract_tool_name tests ───────────────────────────────────────

    #[test]
    fn extract_tool_name_claude() {
        let json = serde_json::json!({ "tool_name": "Edit" });
        assert_eq!(extract_tool_name(&json, HostFormat::Claude), "Edit");
    }

    #[test]
    fn extract_tool_name_antigravity() {
        let json = serde_json::json!({
            "toolCall": { "name": "write_to_file" }
        });
        assert_eq!(
            extract_tool_name(&json, HostFormat::Antigravity),
            "write_to_file",
        );
    }

    #[test]
    fn extract_tool_name_missing() {
        let json = serde_json::json!({});
        assert_eq!(extract_tool_name(&json, HostFormat::Claude), "");
        assert_eq!(extract_tool_name(&json, HostFormat::Antigravity), "");
    }

    // ── Antigravity shell command tests ────────────────────────────────

    #[test]
    fn extract_shell_command_antigravity_run_command() {
        let json = serde_json::json!({
            "toolCall": {
                "name": "run_command",
                "args": { "CommandLine": "make test" }
            }
        });
        assert_eq!(
            extract_shell_command(&json, "run_command", HostFormat::Antigravity),
            Some("make test".to_string()),
        );
    }

    #[test]
    fn extract_shell_command_antigravity_non_shell_returns_none() {
        let json = serde_json::json!({
            "toolCall": {
                "name": "write_to_file",
                "args": { "TargetFile": "/src/main.rs" }
            }
        });
        assert!(extract_shell_command(&json, "write_to_file", HostFormat::Antigravity).is_none(),);
    }

    // ── Teaching payload injection tests (ws36 ticket 01) ─────────────────

    // ── SessionStart source gating ───────────────────────────────────────

    #[test]
    fn session_start_announces_for_startup_clear_compact() {
        assert!(session_start_should_announce(Some("startup")));
        assert!(session_start_should_announce(Some("clear")));
        assert!(session_start_should_announce(Some("compact")));
    }

    #[test]
    fn session_start_skips_resume() {
        assert!(!session_start_should_announce(Some("resume")));
    }

    #[test]
    fn session_start_announces_for_missing_or_unknown_source() {
        // Missing source most likely means a fresh start → inject.
        assert!(session_start_should_announce(None));
        // Unknown source: only `resume` provably already carries the payload,
        // so anything else injects.
        assert!(session_start_should_announce(Some("something-new")));
    }

    // ── Cross-session lingering-worktree line (misc 151 D-2) ──────────

    #[test]
    fn cross_session_orphan_line_counts_present_worktrees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents = tmp.path().join("agents");
        let wt = agents.join("sess-x").join("a");
        std::fs::create_dir_all(&wt).expect("mkdir worktree");
        let meta = crate::worktree_create::WorktreeMeta {
            worktree: wt,
            source_repo: std::path::PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "agent-a".to_string(),
            name: "agent-a".to_string(),
            agent_id: Some("a".to_string()),
            session_id: "sess-x".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: crate::worktree_create::WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        };
        crate::worktree_create::write_sidecar(&meta).expect("write sidecar");

        let line = cross_session_orphan_line(&agents).expect("one lingering worktree");
        assert!(
            line.contains("1 agent worktree lingers"),
            "count line: {line}"
        );
        assert!(line.contains("catenary worktree ls"), "pointer: {line}");
    }

    #[test]
    fn cross_session_orphan_line_none_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // No sidecars → no line.
        assert!(cross_session_orphan_line(&tmp.path().join("agents")).is_none());
    }

    // ── session_start_context: inject/host gating carries the live payload ─

    #[test]
    fn session_start_context_carries_payload_for_claude_inject() {
        let ctx = session_start_context(true, HostFormat::Claude)
            .expect("Claude inject should carry the teaching payload");
        // The static invariants tier is always present regardless of config.
        assert!(
            ctx.contains("The edit→diagnostics loop"),
            "payload body should be inlined: {ctx}",
        );
        // The declared Claude identity keys the misc-177 dispatch section —
        // Claude's installed hook set registers WorktreeCreate.
        assert!(
            ctx.contains("Dispatching isolated work"),
            "Claude session-start payload should teach worktree dispatch: {ctx}"
        );
        assert!(
            ctx.contains("isolation: \"worktree\""),
            "Claude session-start payload should name the isolation flag: {ctx}"
        );
        // Byte-equal to `catenary primer claude` — one rendering, keyed by the
        // declared client, used by both surfaces.
        assert_eq!(
            ctx,
            crate::cli::teaching::emitted_payload(Some(HostFormat::Claude)),
            "session-start payload must be the SSOT claude rendering"
        );
        // No pointer to the on-demand commands — the content is inlined.
        assert!(!ctx.contains("catenary primer"), "no primer pointer: {ctx}");
    }

    #[test]
    fn session_start_context_absent_when_not_announcing() {
        assert!(session_start_context(false, HostFormat::Claude).is_none());
    }

    #[test]
    fn session_start_context_absent_for_other_hosts() {
        // Antigravity and OpenCode carry teaching through other channels — only
        // Claude Code reads `additionalContext` at SessionStart.
        assert!(session_start_context(true, HostFormat::Antigravity).is_none());
        assert!(session_start_context(true, HostFormat::OpenCode).is_none());
    }

    // ── Project-config setup nudge (misc 202) ───────────────────────────

    #[test]
    fn session_start_nudge_read_from_response() {
        // The daemon returns the nudge under `session_start_nudge` on the first
        // response line; the CLI reads it verbatim.
        let lines = vec![
            serde_json::json!({ "session_start_nudge": "rust-analyzer reads rust-analyzer.toml; add one." })
                .to_string(),
        ];
        assert_eq!(
            session_start_nudge_from_response(&lines).as_deref(),
            Some("rust-analyzer reads rust-analyzer.toml; add one."),
        );
    }

    #[test]
    fn session_start_nudge_absent_when_field_missing_or_no_response() {
        // A bare envelope (no nudge field) and an empty response both yield None.
        let envelope_only = vec![serde_json::json!({ "result": null }).to_string()];
        assert_eq!(session_start_nudge_from_response(&envelope_only), None);
        assert_eq!(session_start_nudge_from_response(&[]), None);
        // A non-JSON line is tolerated (None, not a panic).
        assert_eq!(
            session_start_nudge_from_response(&["not json".to_string()]),
            None,
        );
    }

    #[test]
    fn with_project_config_line_augments_only_a_present_context() {
        // Mirrors with_orphan_line: only a Some base is augmented, so a non-Claude
        // host (None context) never receives a standalone Claude-shaped line.
        assert_eq!(
            with_project_config_line(Some("base".to_string()), Some("nudge")),
            Some("base\n\nnudge".to_string()),
        );
        assert_eq!(
            with_project_config_line(Some("base".to_string()), None),
            Some("base".to_string()),
        );
        assert_eq!(with_project_config_line(None, Some("nudge")), None);
        assert_eq!(with_project_config_line(None, None), None);
    }

    #[test]
    fn opencode_session_start_body_is_the_raw_ssot_payload() {
        let body = opencode_session_start_body();
        // Payload parity with the SSOT: byte-equal to the shared emitted payload
        // keyed by the declared OpenCode identity (the same source `catenary
        // primer` and the Claude additionalContext render from — including the
        // daemon-staleness note under the same condition), so the OpenCode
        // instructions file cannot drift.
        assert_eq!(
            body,
            crate::cli::teaching::emitted_payload(Some(HostFormat::OpenCode))
        );
        // OpenCode's hook set carries no WorktreeCreate (yet), so its payload
        // stays the client-neutral one — no misc-177 dispatch section.
        assert!(
            !body.contains("Dispatching isolated work"),
            "opencode body must not carry the dispatch section: {body}"
        );
        // Emitter output shape: raw text, not the Claude structured-output
        // envelope — the plugin writes stdout verbatim into its instructions
        // file.
        assert!(
            !body.contains("hookSpecificOutput"),
            "opencode body must not be the Claude envelope: {body}"
        );
        assert!(
            !body.contains("additionalContext"),
            "opencode body must not be the Claude envelope: {body}"
        );
        assert!(
            !body.trim_start().starts_with('{'),
            "opencode body must be raw text, not JSON: {body}"
        );
        // Carries the invariants tier (payload parity).
        assert!(
            body.contains("The edit→diagnostics loop"),
            "opencode body should inline the invariants: {body}"
        );
    }

    // ── announcement_hook_specific_output shape ──────────────────────────

    #[test]
    fn announcement_hook_specific_output_shape() {
        let session = announcement_hook_specific_output("SessionStart", "BODY");
        assert_eq!(session["hookEventName"], "SessionStart");
        assert_eq!(session["additionalContext"], "BODY");

        let subagent = announcement_hook_specific_output("SubagentStart", "OTHER");
        assert_eq!(subagent["hookEventName"], "SubagentStart");
        assert_eq!(subagent["additionalContext"], "OTHER");
    }

    // ── SessionStart response: additionalContext presence ────────────────

    #[test]
    fn session_start_response_injects_context() {
        let builder = crate::hook::response::SystemMessageBuilder::new();
        let obj = build_session_start_response(builder, Some("BODY"))
            .expect("context should produce an object");
        assert_eq!(obj["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(obj["hookSpecificOutput"]["additionalContext"], "BODY");
        // No systemMessage when the builder is empty.
        assert!(
            obj.get("systemMessage").is_none(),
            "no drain content → no systemMessage",
        );
    }

    #[test]
    fn session_start_response_empty_when_nothing_to_say() {
        let builder = crate::hook::response::SystemMessageBuilder::new();
        // No drain, no context (withheld source / non-Claude) → no object.
        assert!(build_session_start_response(builder, None).is_none());
    }

    #[test]
    fn session_start_resume_still_emits_drain_without_context() {
        use crate::logging::Severity;
        let mut builder = crate::hook::response::SystemMessageBuilder::new();
        builder.push_direct(Severity::Error, "config error");
        // Context withheld (resume) but the drain still surfaces.
        let obj = build_session_start_response(builder, None)
            .expect("drain content should produce an object");
        assert!(
            obj["systemMessage"]
                .as_str()
                .is_some_and(|s| s.contains("config error")),
            "drain content should still surface when context is withheld",
        );
        assert!(
            obj.get("hookSpecificOutput").is_none(),
            "withheld context must not inject additionalContext",
        );
    }

    #[test]
    fn session_start_response_carries_both_fields_in_one_object() {
        use crate::logging::Severity;
        let mut builder = crate::hook::response::SystemMessageBuilder::new();
        builder.push_direct(Severity::Info, "cleared 2 stale editing state entries");
        let obj = build_session_start_response(builder, Some("BODY"))
            .expect("both surfaces should produce an object");
        // ONE object carries both top-level systemMessage and the payload.
        assert!(
            obj["systemMessage"]
                .as_str()
                .is_some_and(|s| s.contains("cleared 2 stale editing state entries")),
            "systemMessage (drain) must be present",
        );
        assert_eq!(obj["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(obj["hookSpecificOutput"]["additionalContext"], "BODY");
        // Exactly the two expected top-level keys.
        let map = obj.as_object().expect("object");
        assert_eq!(map.len(), 2, "exactly systemMessage + hookSpecificOutput");
    }

    // ── SubagentStart: unconditional payload with the per-agent debt line ─

    #[test]
    fn subagent_start_response_announces_payload_for_claude() {
        let obj = build_subagent_start_response(HostFormat::Claude)
            .expect("subagent start should always announce on Claude");
        assert_eq!(obj["hookSpecificOutput"]["hookEventName"], "SubagentStart");
        let ctx = obj["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext string");
        // The emitted subagent payload verbatim (the shared body, the per-agent
        // debt line, and the daemon-staleness note when the daemon is stale).
        // Compared against the same source so the check is deterministic
        // regardless of daemon staleness.
        assert_eq!(ctx, crate::cli::teaching::emitted_subagent_payload());
        // Prefix-identifiable header present (it opens the block, or follows the
        // one-line staleness note when the daemon is stale).
        assert!(
            ctx.contains("Catenary —"),
            "prefix-identifiable header present: {ctx}"
        );
        // The per-agent debt line the SubagentStart variant adds.
        assert!(
            ctx.contains("your diagnostic debt is tracked per-agent"),
            "per-agent debt line present: {ctx}",
        );
        // Deliberately client-neutral (misc 177): the worker does not dispatch
        // isolated work itself, so the dispatch section stays out and the
        // misc-146 mention rides as-is.
        assert!(
            !ctx.contains("Dispatching isolated work"),
            "subagent payload must not carry the dispatch section: {ctx}"
        );
        assert!(
            ctx.contains("Work in isolated subagents"),
            "subagent payload keeps the misc-146 mention: {ctx}"
        );
    }

    #[test]
    fn subagent_start_response_non_claude_is_none() {
        assert!(build_subagent_start_response(HostFormat::Antigravity).is_none());
        assert!(build_subagent_start_response(HostFormat::OpenCode).is_none());
    }

    // ── Antigravity PreInvocation first-sighting injection (ws36 ticket 03) ─

    #[test]
    fn pre_invocation_injection_is_a_persisted_user_message() {
        // The injection is a single persisted `injectSteps` `userMessage` — the
        // analog of the Claude SessionStart `additionalContext`. The transient
        // per-call `ephemeralMessage` channel is excluded by ruling, so it must
        // never appear.
        let out = pre_invocation_injection("BODY");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let steps = v["injectSteps"].as_array().expect("injectSteps array");
        assert_eq!(steps.len(), 1, "exactly one injected step: {out}");
        assert_eq!(steps[0]["userMessage"], "BODY");
        assert!(
            steps[0].get("ephemeralMessage").is_none(),
            "ephemeralMessage is excluded (per-call-only channel): {out}",
        );
    }

    #[test]
    fn pre_invocation_injection_carries_only_the_sliver() {
        // Ticket 14: the `PreInvocation` injection carries only the per-session
        // sliver (the cwd build tool the always-on rules file structurally cannot
        // carry), not the full SSOT payload — that rides the rules file every
        // turn. The injection wrapper carries exactly what it is handed.
        let sliver = "Catenary — this session's workspace specifics (...):\nBuild tool: make";
        let out = pre_invocation_injection(sliver);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(
            v["injectSteps"][0]["userMessage"]
                .as_str()
                .expect("userMessage string"),
            sliver,
        );
        // The full teaching invariants are NOT here — they live on the rules file.
        assert!(
            !v["injectSteps"][0]["userMessage"]
                .as_str()
                .is_some_and(|s| s.contains("The edit→diagnostics loop")),
            "the sliver must not carry the full invariants: {out}",
        );
    }

    #[test]
    fn empty_pre_invocation_injects_nothing() {
        // The non-first-sighting / fail-closed output: a bare JSON object with no
        // `injectSteps`, so a second invocation (and every later one) injects
        // nothing.
        let out = empty_pre_invocation();
        assert_eq!(out, "{}");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(
            v.as_object().expect("object").is_empty(),
            "no injectSteps on the no-op path: {out}",
        );
    }

    // ── Host Grep/Glob deny levers (misc 201, component 2) ───────────────

    #[test]
    fn host_grep_glob_untouched_by_default() {
        // Default policy — both levers off. The host tools pass through untouched
        // (no denial), so a user who has not opted in keeps the host Grep/Glob.
        let perms = crate::config::PermissionsConfig::default();
        assert!(host_search_tool_denial_for("Grep", &perms).is_none());
        assert!(host_search_tool_denial_for("Glob", &perms).is_none());
    }

    #[test]
    fn host_grep_denied_with_redirect_teaching_when_on() {
        let perms = crate::config::PermissionsConfig {
            deny_host_grep: true,
            ..Default::default()
        };
        let msg = host_search_tool_denial_for("Grep", &perms).expect("Grep denied");
        assert_eq!(
            msg,
            "`Grep` isn't allowed. Use `catenary grep` instead. Works on any path (LSP enrichment only within tracked roots)."
        );
        // The glob lever is independent — Glob stays untouched.
        assert!(host_search_tool_denial_for("Glob", &perms).is_none());
    }

    #[test]
    fn host_glob_denied_with_redirect_teaching_when_on() {
        let perms = crate::config::PermissionsConfig {
            deny_host_glob: true,
            ..Default::default()
        };
        let msg = host_search_tool_denial_for("Glob", &perms).expect("Glob denied");
        assert_eq!(
            msg,
            "`Glob` isn't allowed. Use `catenary glob` instead. Works on any path (LSP enrichment only within tracked roots)."
        );
        assert!(host_search_tool_denial_for("Grep", &perms).is_none());
    }

    #[test]
    fn host_deny_lever_ignores_non_search_tools() {
        let perms = crate::config::PermissionsConfig {
            deny_host_grep: true,
            deny_host_glob: true,
            ..Default::default()
        };
        // Read/Write/Bash are never host search tools — the lever never touches them.
        assert!(host_search_tool_denial_for("Read", &perms).is_none());
        assert!(host_search_tool_denial_for("Write", &perms).is_none());
        assert!(host_search_tool_denial_for("Bash", &perms).is_none());
    }

    // ── PostToolUse redaction backstop (misc 201, component 3) ───────────

    #[test]
    fn redact_tool_output_clean_output_emits_nothing() {
        // A clean tool_response: no hit, no emission — the output passes through
        // byte-identical (no updatedToolOutput at all).
        let payload = serde_json::json!({
            "tool_name": "Read",
            "tool_response": "fn main() { println!(\"hello\"); }",
        });
        assert!(!redact_tool_output(&payload));
    }

    #[test]
    fn redact_tool_output_string_response_redacts_secret() {
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_response": "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
        });
        // A live pin against a real Claude Code session is the follow-up (the
        // exact updatedToolOutput field name is docs-level evidence); here we
        // assert the emission fires on a hit.
        assert!(redact_tool_output(&payload));
    }

    #[test]
    fn redact_tool_output_absent_response_emits_nothing() {
        // No tool_response field — nothing to scan, nothing emitted.
        let payload = serde_json::json!({ "tool_name": "Read" });
        assert!(!redact_tool_output(&payload));
    }
}
