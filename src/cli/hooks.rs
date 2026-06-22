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

#![allow(
    clippy::print_stdout,
    reason = "hook output is stdout JSON for the host CLI"
)]

use std::path::PathBuf;
use std::time::Duration;

use crate::cli::HostFormat;

// ── Surface announcement (ticket 04) ────────────────────────────────────

/// The Catenary surface announcement, injected into the agent's context at
/// every context discontinuity (`SessionStart` for `startup`/`clear`/`compact`,
/// and every `SubagentStart`).
///
/// A pointer, not a payload: it names `catenary primer` and `catenary commands`
/// rather than inlining a surface description that drifts. The two commands
/// regenerate from the live binary, so the injected context can never go stale.
///
/// Single source of truth — both call sites share this constant so the wording
/// cannot drift between them.
const SURFACE_ANNOUNCEMENT: &str = "Catenary exposes a code-intelligence tool surface through the `catenary` CLI (not via MCP) and enforces editing and command policy with a hook that runs before each tool use. Run `catenary primer` for the tool reference and the edit→diagnostics loop; run `catenary commands` for the allowed/denied shell-command surface.";

/// Returns the verbatim Catenary surface announcement.
///
/// Shared by the `SessionStart` and `SubagentStart` hook handlers so the
/// wording is defined once.
#[must_use]
const fn surface_announcement() -> &'static str {
    SURFACE_ANNOUNCEMENT
}

/// Decide whether a `SessionStart` hook should inject the surface announcement,
/// given the payload's `source` field.
///
/// The announcement is injected whenever the new context does *not* already
/// contain it:
/// - `startup` (fresh), `clear` (`/clear` wiped it), `compact` (summarization
///   may have dropped it) → **inject**.
/// - `resume` → **skip** — resume restores the prior transcript verbatim, so the
///   announcement is already present.
///
/// A missing or unknown `source` is treated as **inject**: a missing source most
/// likely means a fresh start, and only `resume` is a context that provably
/// already carries the announcement.
#[must_use]
fn session_start_should_announce(source: Option<&str>) -> bool {
    source != Some("resume")
}

/// Build the `hookSpecificOutput` object that carries the surface announcement
/// in its `additionalContext` field, for the given hook event.
///
/// `hook_event_name` is `"SessionStart"` or `"SubagentStart"` — the Claude Code
/// structured-output shape is
/// `{"hookSpecificOutput": {"hookEventName": <event>, "additionalContext": <string>}}`.
/// This builds only the inner object; callers nest it under `hookSpecificOutput`.
#[must_use]
fn announcement_hook_specific_output(hook_event_name: &str) -> serde_json::Value {
    serde_json::json!({
        "hookEventName": hook_event_name,
        "additionalContext": surface_announcement(),
    })
}

/// Connect to the daemon's IPC endpoint.
///
/// When the socket file exists but the connection fails (daemon likely
/// crashed), emits an `error!()` event. The hook CLI's tracing subscriber
/// routes this to a desktop notification so the user gets an immediate
/// signal outside the agent's context.
#[cfg(unix)]
fn hook_connect(_hook_json: &serde_json::Value) -> Option<std::os::unix::net::UnixStream> {
    let daemon_path = crate::router::socket_path();
    let result = notify_connect(&daemon_path);
    if result.is_none() && daemon_path.exists() {
        // Socket exists but connection failed — daemon likely crashed.
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
        HostFormat::Gemini | HostFormat::Antigravity => serde_json::json!({
            "decision": "deny",
            "reason": reason
        })
        .to_string(),
    }
}

/// Format a Stop/AfterAgent block response for the host CLI.
fn format_stop_block(reason: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude => serde_json::json!({
            "decision": "block",
            "reason": reason
        })
        .to_string(),
        HostFormat::Gemini => serde_json::json!({
            "decision": "retry",
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
        HostFormat::Claude | HostFormat::Gemini => hook_json.get("session_id"),
        HostFormat::Antigravity => hook_json.get("conversationId"),
    }
    .and_then(|v| v.as_str())
}

/// Extract working directory from hook payload.
///
/// Each host uses a different field name and shape for the working directory.
fn extract_cwd_str(hook_json: &serde_json::Value, format: HostFormat) -> Option<&str> {
    match format {
        HostFormat::Claude | HostFormat::Gemini => hook_json.get("cwd").and_then(|v| v.as_str()),
        HostFormat::Antigravity => hook_json
            .get("workspacePaths")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str()),
    }
}

/// Extract tool name from hook payload.
///
/// Each host uses a different field path for the tool name.
fn extract_tool_name(hook_json: &serde_json::Value, format: HostFormat) -> &str {
    match format {
        HostFormat::Claude | HostFormat::Gemini => {
            hook_json.get("tool_name").and_then(|v| v.as_str())
        }
        HostFormat::Antigravity => hook_json
            .get("toolCall")
            .and_then(|tc| tc.get("name"))
            .and_then(|v| v.as_str()),
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
        HostFormat::Claude | HostFormat::Gemini => hook_json
            .get("tool_input")
            .and_then(|ti| ti.get("file_path").or_else(|| ti.get("file")))
            .and_then(|fp| fp.as_str()),
        HostFormat::Antigravity => hook_json
            .get("toolCall")
            .and_then(|tc| tc.get("args"))
            .and_then(|a| a.get("TargetFile"))
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
/// invalid, surfaces a `systemMessage` directing the user to
/// `catenary doctor`, combined with any background notifications from the
/// notification queue drain.
pub fn run_session_start(format: HostFormat) {
    use crate::hook::response::SystemMessageBuilder;
    use crate::logging::Severity;

    let mut builder = SystemMessageBuilder::new();

    // Config validation — runs before IPC, no session needed.
    if let Err(e) = crate::config::Config::check() {
        builder.push_direct(
            Severity::Error,
            &format!("Catenary configuration error: {e:#}. Run `catenary doctor` for details."),
        );
    }

    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        emit_session_start(builder, false, format);
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        emit_session_start(builder, false, format);
        return;
    };

    // Source gating (ticket 04): inject the surface announcement unless the
    // context provably already carries it (`resume`). `source` is read here,
    // CLI-side, from the full hook payload.
    let source = hook_json.get("source").and_then(|v| v.as_str());
    let announce = session_start_should_announce(source);

    let Some(stream) = hook_connect(&hook_json) else {
        emit_session_start(builder, announce, format);
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

    let lines = ipc_exchange(stream, &request);

    if let Some(line) = lines.first()
        && let Ok(envelope) = serde_json::from_str::<crate::hook::HookResponseEnvelope>(line)
    {
        if let Some(crate::hook::HookResult::Cleared(count)) = &envelope.result {
            builder.push_direct(
                Severity::Info,
                &format!("Catenary: cleared {count} stale editing state entries"),
            );
        }
        if let Some(bg) = envelope.system_message {
            // Server-side background drain content: each line is a
            // pre-rendered notification. Add them as background lines.
            for bg_line in bg.lines() {
                // Skip the header — the builder adds its own.
                if !bg_line.starts_with("───") {
                    builder.push_background(bg_line.to_string());
                }
            }
        }
    }

    emit_session_start(builder, announce, format);
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

/// Mount a subagent's worktree as a workspace root and announce the Catenary
/// surface (`SubagentStart` hook handler).
///
/// Fires once at subagent spawn. Two **decoupled** effects:
/// - **Mount (conditional):** forwards `cwd` (the worktree of an
///   `isolation:"worktree"` subagent) and `session_id` to the daemon, which
///   mounts the worktree under `worktree:{session_id}:{path}` iff its canonical
///   project root is already tracked (the Explore/Plan self-scoping is enforced
///   daemon-side). Best effort — the host CLI ignores all flow-control fields.
/// - **Announcement (unconditional, ticket 04):** every subagent spawn is a
///   fresh context that never had the announcement, so the surface announcement
///   is emitted via `hookSpecificOutput.additionalContext` for **every**
///   subagent, regardless of whether a worktree was mounted. This is a Claude
///   Code channel; no other supported host spawns subagents.
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
    // announcement below — most subagents mount nothing but all are announced.
    if let Some(stream) = hook_connect(&hook_json) {
        let mut request = serde_json::json!({
            "method": "subagent-start/mount-worktree",
            "format": format.as_str(),
        });
        if let Some(sid) = extract_session_id(&hook_json, format) {
            request["session_id"] = serde_json::json!(sid);
        }
        if let Some(cwd) = extract_cwd_str(&hook_json, format) {
            request["cwd"] = serde_json::json!(cwd);
        }

        // Fire and forget — no response processing needed.
        let _ = ipc_exchange(stream, &request);
    }

    emit_subagent_start_announcement(format);
}

/// Emit the unconditional `SubagentStart` surface announcement.
///
/// Prints a single object carrying `hookSpecificOutput.additionalContext` for
/// Claude; other hosts get nothing (no other supported host spawns subagents).
fn emit_subagent_start_announcement(format: HostFormat) {
    if let Some(obj) = build_subagent_start_response(format) {
        print!("{obj}");
    }
}

/// Build the `SubagentStart` hook-response object carrying the surface
/// announcement, or `None` for non-Claude hosts.
fn build_subagent_start_response(format: HostFormat) -> Option<serde_json::Value> {
    if !matches!(format, HostFormat::Claude) {
        return None;
    }
    Some(serde_json::json!({
        "hookSpecificOutput": announcement_hook_specific_output("SubagentStart"),
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

/// Build the single `SessionStart` hook-response object.
///
/// Carries up to two coexisting surfaces in **one** JSON object:
/// - a top-level `systemMessage` (the user-facing notification drain), present
///   when the builder has content;
/// - `hookSpecificOutput.additionalContext` (the silent surface announcement),
///   present when `announce` is true (source gating, ticket 04) and the host is
///   Claude — `additionalContext` is a Claude Code channel.
///
/// Returns `None` when neither surface has content, so nothing is emitted.
fn build_session_start_response(
    builder: crate::hook::response::SystemMessageBuilder,
    announce: bool,
    format: HostFormat,
) -> Option<serde_json::Value> {
    let system_message = builder.finish();
    let inject = announce && matches!(format, HostFormat::Claude);

    if system_message.is_none() && !inject {
        return None;
    }

    let mut obj = serde_json::Map::new();
    if let Some(msg) = system_message {
        obj.insert("systemMessage".to_string(), serde_json::Value::String(msg));
    }
    if inject {
        obj.insert(
            "hookSpecificOutput".to_string(),
            announcement_hook_specific_output("SessionStart"),
        );
    }
    Some(serde_json::Value::Object(obj))
}

/// Finalize and print the `SessionStart` hook response (notification drain +
/// surface announcement) as a single JSON object.
fn emit_session_start(
    builder: crate::hook::response::SystemMessageBuilder,
    announce: bool,
    format: HostFormat,
) {
    if let Some(obj) = build_session_start_response(builder, announce, format) {
        print!("{obj}");
    }
}

/// Format a `systemMessage` for hook responses.
fn format_system_message(msg: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude | HostFormat::Gemini | HostFormat::Antigravity => {
            serde_json::json!({ "systemMessage": msg }).to_string()
        }
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
    {
        if let Some(crate::hook::HookResult::Block(reason)) = &envelope.result {
            // Blocking: notifications stay queued (server didn't drain).
            print!("{}", format_stop_block(reason, format));
        } else if let Some(sys_msg) = &envelope.system_message {
            // Allowing with background notifications.
            print!("{}", format_system_message(sys_msg, format));
        }
    }
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

    // ── Catenary CLI commands (regime 1: canonical-form matcher) ──
    // Recognize and classify Catenary's own subcommands in any position
    // (ADR 013). `grep`/`glob`/`sed`/`diagnostics`/`roots`/`primer` are
    // allowed only in canonical form (no pipe/redirect/substitution/
    // background; `diagnostics`/`sed` and the editing lifecycle must be
    // bare); non-agent commands and unrecognized subcommands are denied
    // with a pedagogical message.
    if let Some(ref shell_cmd) = extract_shell_command(&hook_json, tool_name, format) {
        use crate::cli::command_filter::CatenaryAction;
        match crate::cli::command_filter::analyze_catenary_command(shell_cmd) {
            CatenaryAction::Deny(reason) => {
                print!("{}", format_deny(&reason, format));
                return;
            }
            // `editing start` and `diagnostics` send IPC to the daemon, then
            // allow the command to execute (it prints confirmation /
            // diagnostics). The internal handoff method (`pre-tool/editing-stop`)
            // is unchanged — only the user-facing command was renamed.
            CatenaryAction::EditingStart => {
                handle_start_editing_hook(&hook_json, format);
                return;
            }
            CatenaryAction::Diagnostics => {
                handle_done_editing_hook(&hook_json, format);
                return;
            }
            // `catenary sed --in-place` writes through the daemon, which knows
            // the runtime-changed set but not the agent's identity. Stage the
            // identity forward (`pre-tool/sed`) so the daemon can accumulate the
            // swept files. A preview (`in_place = false`) is a stateless query —
            // nothing to stage; just allow it.
            CatenaryAction::Sed { in_place } => {
                if in_place {
                    handle_sed_hook(&hook_json, format);
                }
                return;
            }
            // Canonical search/tool command. A `cd`/foreign-chained search
            // (or an arg-substitution carrying a foreign command) still has
            // its foreign segments allowlist-checked (regime 2); `catenary`
            // segments are skipped by `check_command`. Always allowed,
            // including during editing, so we never fall through to
            // editing-state enforcement.
            CatenaryAction::Allow { has_foreign } => {
                if has_foreign
                    && let Some(reason) = foreign_command_denial(&hook_json, shell_cmd, format)
                {
                    print!("{}", format_deny(&reason, format));
                }
                return;
            }
            CatenaryAction::NotCatenary => {}
        }
    }

    // ── Command filter (regime 2: foreign allowlist) ─────────────
    // Try session-side check first (full multi-root merged config).
    // Fall back to client-side check (user config + cwd's project
    // config) when the session is unreachable.
    if let Some(shell_cmd) = extract_shell_command(&hook_json, tool_name, format)
        && let Some(reason) = foreign_command_denial(&hook_json, &shell_cmd, format)
    {
        print!("{}", format_deny(&reason, format));
        return;
    }

    // ── Editing state enforcement (IPC to daemon / session) ──────
    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let file_path = extract_file_path(&hook_json, format);
    let agent_id = extract_agent_id(&hook_json);
    let session_id = extract_session_id(&hook_json, format);
    let shell_cmd = extract_shell_command(&hook_json, tool_name, format);

    let mut request = serde_json::json!({
        "method": "pre-tool/editing-state",
        "tool_name": tool_name,
        "agent_id": agent_id,
        "format": format.as_str(),
    });
    if let Some(path) = &file_path {
        request["file_path"] = serde_json::json!(path);
    }
    if let Some(cmd) = &shell_cmd {
        request["command"] = serde_json::json!(cmd);
    }
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }
    request["host_payload"] = prepare_host_payload(&hook_json);

    let lines = ipc_exchange(stream, &request);

    if let Some(line) = lines.first()
        && let Ok(envelope) = serde_json::from_str::<crate::hook::HookResponseEnvelope>(line)
        && let Some(crate::hook::HookResult::Deny(reason)) = &envelope.result
    {
        print!("{}", format_deny(reason, format));
    }
}

/// Run the foreign-command allowlist filter (regime 2) and return the formatted
/// denial reason, or `None` when the command is allowed.
///
/// Tries the session-side check (full merged config) first, then the
/// client-side fallback (user config + cwd project config). Catenary's own
/// commands are skipped by [`check_command`](crate::cli::command_filter::check_command)
/// — they run under the canonical-form matcher (regime 1), not the allowlist.
fn foreign_command_denial(
    hook_json: &serde_json::Value,
    shell_cmd: &str,
    format: HostFormat,
) -> Option<String> {
    if let Some(reason) = ipc_check_command(hook_json, shell_cmd, format) {
        return Some(reason);
    }
    // IPC failed or session unreachable — try client-side.
    let (denial, resolved) = check_shell_command(hook_json, shell_cmd, format)?;
    let build_hint = resolve_client_build_hint(hook_json, &denial.command, &resolved, format);
    Some(crate::cli::command_filter::format_denial(
        &denial.command,
        &resolved,
        &denial,
        Some(format),
        build_hint.as_deref(),
    ))
}

/// Check a shell command against the configured allowlist.
///
/// Loads user config, then merges with the `cwd`'s project config (if any)
/// for per-root `build` tool support. This is a client-side fallback — the
/// full session-side check (all roots, dynamically-added roots) is handled
/// by `pre-tool/check-command` IPC in ticket 03a.
///
/// Returns a [`Denial`](crate::cli::command_filter::Denial) and the resolved
/// config on denial, or `None` if the command is allowed.
fn check_shell_command(
    hook_json: &serde_json::Value,
    cmd: &str,
    format: HostFormat,
) -> Option<(
    crate::cli::command_filter::Denial,
    crate::config::ResolvedCommands,
)> {
    let config = crate::config::Config::load().ok()?;
    let mut resolved = config.resolved_commands?;
    if resolved.client_enforcement_only {
        return None;
    }

    // Merge with cwd's project config for per-root build support.
    // Walk up from cwd to find the nearest `.catenary.toml` — cwd is
    // typically a subdirectory of the workspace root.
    // This covers the common single-root case and "agent is in the right
    // directory" case. Multi-root coverage requires the session-side check.
    let cwd = extract_cwd_str(hook_json, format).map(PathBuf::from);
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
        return None;
    }

    let denial = crate::cli::command_filter::check_command(cmd, &resolved, cwd.as_deref())?;
    Some((denial, resolved))
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

/// Session-side command check via IPC.
///
/// Sends `pre-tool/check-command` with the shell command and cwd. The
/// session evaluates against the merged allowlist (all roots, all project
/// configs) and handles debounce. Returns the denial reason string on
/// denial, `None` on allow or IPC failure.
fn ipc_check_command(
    hook_json: &serde_json::Value,
    shell_cmd: &str,
    format: HostFormat,
) -> Option<String> {
    let stream = hook_connect(hook_json)?;

    let cwd = extract_cwd_str(hook_json, format);
    let session_id = extract_session_id(hook_json, format);

    let mut request = serde_json::json!({
        "method": "pre-tool/check-command",
        "command": shell_cmd,
        "format": format.as_str(),
    });
    if let Some(c) = cwd {
        request["cwd"] = serde_json::json!(c);
    }
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }
    request["host_payload"] = prepare_host_payload(hook_json);

    let lines = ipc_exchange(stream, &request);
    let line = lines.first()?;
    let envelope = serde_json::from_str::<crate::hook::HookResponseEnvelope>(line).ok()?;
    match envelope.result {
        Some(crate::hook::HookResult::Deny(reason)) => Some(reason),
        _ => None,
    }
}

/// Extract the shell command string from hook JSON for Bash-like tools.
///
/// Returns `Some(command)` for Claude Code's `Bash` tool and Gemini CLI's
/// `run_shell_command` tool. Returns `None` for all other tools.
fn extract_shell_command(
    hook_json: &serde_json::Value,
    tool_name: &str,
    format: HostFormat,
) -> Option<String> {
    let is_shell_tool = match format {
        HostFormat::Claude => tool_name == "Bash",
        HostFormat::Gemini => tool_name == "run_shell_command",
        HostFormat::Antigravity => tool_name == "run_command",
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
        // Claude Code / Gemini CLI: "command"; Antigravity CLI: "CommandLine"
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

/// Handle `PreToolUse` for `catenary diagnostics`.
///
/// Sends `pre-tool/editing-stop` IPC to the daemon to prepare the handoff
/// (the internal method name is unchanged by the user-facing rename): drain
/// accumulated files, release the editing guardrail, and deposit the file
/// list in the handoff slot. Returns allow (silent) or deny (prints denial
/// reason to stdout for the host CLI).
fn handle_done_editing_hook(hook_json: &serde_json::Value, format: HostFormat) {
    let Some(stream) = hook_connect(hook_json) else {
        return;
    };

    let agent_id = extract_agent_id(hook_json);
    let session_id = extract_session_id(hook_json, format);

    let mut request = serde_json::json!({
        "method": "pre-tool/editing-stop",
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

/// Handle `PreToolUse` for `catenary sed --in-place`.
///
/// Sends `pre-tool/sed` IPC to the daemon to stage the *identity-forward*
/// handoff: the hook holds `(session_id, agent_id)` but not the runtime-changed
/// set, so it deposits the identity under the `sed` key. The sed process then
/// connects, performs the write, and the daemon accumulates the changed files
/// under the staged identity. Allow (silent) — the `catenary sed` command runs.
fn handle_sed_hook(hook_json: &serde_json::Value, format: HostFormat) {
    let Some(stream) = hook_connect(hook_json) else {
        return;
    };

    let agent_id = extract_agent_id(hook_json);
    let session_id = extract_session_id(hook_json, format);

    let mut request = serde_json::json!({
        "method": "pre-tool/sed",
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};

    #[test]
    fn test_format_system_message_claude() -> Result<()> {
        let output =
            format_system_message("─── background ───\n[warn] ra offline", HostFormat::Claude);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(
            parsed["systemMessage"].as_str(),
            Some("─── background ───\n[warn] ra offline"),
        );
        Ok(())
    }

    #[test]
    fn test_format_system_message_gemini() -> Result<()> {
        let output = format_system_message(
            "─── background ───\n[err] pylsp crashed",
            HostFormat::Gemini,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(
            parsed["systemMessage"].as_str(),
            Some("─── background ───\n[err] pylsp crashed"),
        );
        Ok(())
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
    fn extract_shell_command_gemini_run_shell() {
        let json = serde_json::json!({
            "tool_name": "run_shell_command",
            "tool_input": { "command": "make test" }
        });
        assert_eq!(
            extract_shell_command(&json, "run_shell_command", HostFormat::Gemini),
            Some("make test".to_string()),
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
        // Bash tool name with Gemini format → not a shell tool
        let json = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "ls" }
        });
        assert!(extract_shell_command(&json, "Bash", HostFormat::Gemini).is_none());
    }

    #[test]
    fn extract_shell_command_gemini_args_fallback() {
        let json = serde_json::json!({
            "tool_name": "run_shell_command",
            "args": { "command": "git status" }
        });
        assert_eq!(
            extract_shell_command(&json, "run_shell_command", HostFormat::Gemini),
            Some("git status".to_string()),
        );
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

    #[test]
    fn format_deny_gemini_structure() -> Result<()> {
        let output = format_deny("not on allowlist", HostFormat::Gemini);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["decision"], "deny");
        assert_eq!(parsed["reason"], "not on allowlist");
        // Gemini format should NOT have hookSpecificOutput.hookEventName
        assert!(parsed["hookSpecificOutput"].is_null());
        Ok(())
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

    #[test]
    fn format_stop_block_gemini_structure() -> Result<()> {
        let output = format_stop_block("editing not released", HostFormat::Gemini);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["decision"], "retry");
        assert_eq!(parsed["reason"], "editing not released");
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
            extract_file_path(&json, HostFormat::Gemini),
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
            "[language.rust]\nservers = [\"rust-analyzer\"]\n",
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
            "[language.python]\nservers = [\"pyright\"]\n",
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

    #[test]
    fn antigravity_system_message_format() -> Result<()> {
        let output = format_system_message("config error", HostFormat::Antigravity);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["systemMessage"].as_str(), Some("config error"));
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

    // ── Surface announcement tests (ticket 04) ────────────────────────────

    /// The expected verbatim announcement, mirrored here so a wording change
    /// must be made deliberately in two places.
    const EXPECTED_ANNOUNCEMENT: &str = "Catenary exposes a code-intelligence tool surface through the `catenary` CLI (not via MCP) and enforces editing and command policy with a hook that runs before each tool use. Run `catenary primer` for the tool reference and the edit→diagnostics loop; run `catenary commands` for the allowed/denied shell-command surface.";

    #[test]
    fn surface_announcement_is_verbatim() {
        assert_eq!(surface_announcement(), EXPECTED_ANNOUNCEMENT);
    }

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
        // Unknown source: only `resume` provably already carries the
        // announcement, so anything else injects.
        assert!(session_start_should_announce(Some("something-new")));
    }

    // ── announcement_hook_specific_output shape ──────────────────────────

    #[test]
    fn announcement_hook_specific_output_shape() {
        let session = announcement_hook_specific_output("SessionStart");
        assert_eq!(session["hookEventName"], "SessionStart");
        assert_eq!(session["additionalContext"], EXPECTED_ANNOUNCEMENT);

        let subagent = announcement_hook_specific_output("SubagentStart");
        assert_eq!(subagent["hookEventName"], "SubagentStart");
        assert_eq!(subagent["additionalContext"], EXPECTED_ANNOUNCEMENT);
    }

    // ── SessionStart response: additionalContext presence by source ──────

    #[test]
    fn session_start_response_injects_for_inject_sources() {
        for source in ["startup", "clear", "compact"] {
            let builder = crate::hook::response::SystemMessageBuilder::new();
            let announce = session_start_should_announce(Some(source));
            let obj = build_session_start_response(builder, announce, HostFormat::Claude)
                .expect("announcement should produce an object");
            assert_eq!(
                obj["hookSpecificOutput"]["hookEventName"], "SessionStart",
                "source {source} should set hookEventName",
            );
            assert_eq!(
                obj["hookSpecificOutput"]["additionalContext"], EXPECTED_ANNOUNCEMENT,
                "source {source} should inject the announcement",
            );
            // No systemMessage when the builder is empty.
            assert!(
                obj.get("systemMessage").is_none(),
                "no drain content → no systemMessage",
            );
        }
    }

    #[test]
    fn session_start_response_skips_announcement_for_resume() {
        let builder = crate::hook::response::SystemMessageBuilder::new();
        let announce = session_start_should_announce(Some("resume"));
        // Empty builder + no announcement → nothing emitted.
        assert!(
            build_session_start_response(builder, announce, HostFormat::Claude).is_none(),
            "resume with no drain content should emit nothing",
        );
    }

    #[test]
    fn session_start_resume_still_emits_drain_without_announcement() {
        use crate::logging::Severity;
        let mut builder = crate::hook::response::SystemMessageBuilder::new();
        builder.push_direct(Severity::Error, "config error");
        let announce = session_start_should_announce(Some("resume"));
        let obj = build_session_start_response(builder, announce, HostFormat::Claude)
            .expect("drain content should produce an object");
        // systemMessage present (the drain), but no announcement on resume.
        assert!(
            obj["systemMessage"]
                .as_str()
                .is_some_and(|s| s.contains("config error")),
            "drain content should still surface on resume",
        );
        assert!(
            obj.get("hookSpecificOutput").is_none(),
            "resume must not inject additionalContext",
        );
    }

    #[test]
    fn session_start_response_carries_both_fields_in_one_object() {
        use crate::logging::Severity;
        let mut builder = crate::hook::response::SystemMessageBuilder::new();
        builder.push_direct(Severity::Info, "cleared 2 stale editing state entries");
        let obj = build_session_start_response(builder, true, HostFormat::Claude)
            .expect("both surfaces should produce an object");
        // ONE object carries both top-level systemMessage and the announcement.
        assert!(
            obj["systemMessage"]
                .as_str()
                .is_some_and(|s| s.contains("cleared 2 stale editing state entries")),
            "systemMessage (drain) must be present",
        );
        assert_eq!(obj["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(
            obj["hookSpecificOutput"]["additionalContext"],
            EXPECTED_ANNOUNCEMENT,
        );
        // Exactly the two expected top-level keys.
        let map = obj.as_object().expect("object");
        assert_eq!(map.len(), 2, "exactly systemMessage + hookSpecificOutput");
    }

    #[test]
    fn session_start_response_empty_when_nothing_to_say() {
        let builder = crate::hook::response::SystemMessageBuilder::new();
        // No drain, no announcement → no object.
        assert!(build_session_start_response(builder, false, HostFormat::Claude).is_none());
    }

    #[test]
    fn session_start_response_non_claude_omits_announcement() {
        // additionalContext is a Claude Code channel; other hosts get only the
        // drain, never the announcement.
        let builder = crate::hook::response::SystemMessageBuilder::new();
        assert!(
            build_session_start_response(builder, true, HostFormat::Gemini).is_none(),
            "non-Claude host with no drain emits nothing even when announce=true",
        );
    }

    // ── SubagentStart: unconditional announcement ────────────────────────

    #[test]
    fn subagent_start_response_always_announces_for_claude() {
        let obj = build_subagent_start_response(HostFormat::Claude)
            .expect("subagent start should always announce on Claude");
        assert_eq!(obj["hookSpecificOutput"]["hookEventName"], "SubagentStart");
        assert_eq!(
            obj["hookSpecificOutput"]["additionalContext"],
            EXPECTED_ANNOUNCEMENT,
        );
    }

    #[test]
    fn subagent_start_response_non_claude_is_none() {
        assert!(build_subagent_start_response(HostFormat::Gemini).is_none());
        assert!(build_subagent_start_response(HostFormat::Antigravity).is_none());
    }
}
