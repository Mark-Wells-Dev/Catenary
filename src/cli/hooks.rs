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
//! - `run_pre_agent` — root sync (`UserPromptSubmit` / `BeforeAgent`)
//! - `run_pre_tool` — editing state enforcement (`PreToolUse` / `BeforeTool`)
//! - `run_post_tool` — diagnostics (`PostToolUse` / `AfterTool`)
//! - `run_post_agent` — force `done_editing` (`Stop` / `AfterAgent`)
//! - `run_session_start` — clear stale editing state (`SessionStart`)

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use std::path::PathBuf;
use std::time::Duration;

use crate::cli::HostFormat;
use crate::{db, session};

/// Per-session IPC endpoint path (legacy fallback).
///
/// Used when the daemon hook socket is not available.
fn legacy_hook_endpoint(session_id: &str) -> PathBuf {
    #[cfg(unix)]
    {
        session::sessions_dir().join(session_id).join("notify.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\catenary-{session_id}"))
    }
}

/// Find the Catenary session ID for a hook payload.
///
/// Prefers matching the host CLI's `session_id` against the stored
/// `client_session_id` on alive sessions (stable across cwd changes).
/// Falls back to cwd-based workspace prefix matching when no
/// `session_id` is present or no match is found (needed for
/// `SessionStart` bootstrapping before `client_session_id` is stored).
fn find_session_id(hook_json: &serde_json::Value, conn: &rusqlite::Connection) -> Option<String> {
    let sessions = session::list_sessions_with_conn(conn).unwrap_or_default();

    // Primary: match by client_session_id.
    if let Some(host_session_id) = hook_json.get("session_id").and_then(|v| v.as_str())
        && let Some((s, _)) = sessions
            .iter()
            .find(|(s, alive)| *alive && s.client_session_id.as_deref() == Some(host_session_id))
    {
        return Some(s.id.clone());
    }

    // Fallback: cwd-based workspace prefix matching.
    let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let cwd_str = cwd.to_string_lossy();
    sessions
        .into_iter()
        .find(|(s, alive)| *alive && is_path_prefix(&cwd_str, &s.workspace))
        .map(|(s, _)| s.id)
}

/// Path-component-aware prefix check.
///
/// Returns `true` if `path` starts with `prefix` at a path boundary:
/// exact match, or the character after the prefix is `/`. Plain string
/// prefix matching would let `/home/user/Catenary` match a cwd of
/// `/home/user/Catenary-06`, routing hooks to the wrong session.
fn is_path_prefix(path: &str, prefix: &str) -> bool {
    if path == prefix {
        return true;
    }
    path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
}

/// Connect to a hook IPC endpoint.
///
/// Tries the daemon hook socket first. Falls back to per-session
/// socket discovery when the daemon is not running.
#[cfg(unix)]
fn hook_connect(hook_json: &serde_json::Value) -> Option<std::os::unix::net::UnixStream> {
    let daemon_path = crate::router::hook_socket_path();
    if let Some(stream) = notify_connect(&daemon_path) {
        return Some(stream);
    }
    let conn = db::open_and_migrate().ok()?;
    let sid = find_session_id(hook_json, &conn)?;
    notify_connect(&legacy_hook_endpoint(&sid))
}

/// Connect to a hook IPC endpoint (Windows fallback).
///
/// Daemon is Unix-only; uses per-session socket discovery directly.
#[cfg(windows)]
fn hook_connect(hook_json: &serde_json::Value) -> Option<std::fs::File> {
    let conn = db::open_and_migrate().ok()?;
    let sid = find_session_id(hook_json, &conn)?;
    notify_connect(&legacy_hook_endpoint(&sid))
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
        HostFormat::Gemini => serde_json::json!({
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
    }
}

/// Extract `agent_id` from hook payload. Defaults to empty string (main agent).
fn extract_agent_id(hook_json: &serde_json::Value) -> &str {
    hook_json
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// Extracts the file path from hook JSON's `tool_input`.
fn extract_file_path(hook_json: &serde_json::Value) -> Option<String> {
    let file_path = hook_json
        .get("tool_input")
        .and_then(|ti| ti.get("file_path").or_else(|| ti.get("file")))
        .and_then(|fp| fp.as_str())?;

    // Resolve to absolute path
    let abs_path = if std::path::Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );
        cwd.join(file_path)
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
        emit_system_message(builder, format);
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        emit_system_message(builder, format);
        return;
    };

    let Some(stream) = hook_connect(&hook_json) else {
        emit_system_message(builder, format);
        return;
    };

    let session_id = hook_json.get("session_id").and_then(|v| v.as_str());
    let mut request = serde_json::json!({"method": "session-start/clear-editing"});
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

    emit_system_message(builder, format);
}

/// Clean up session state on exit (`SessionEnd` hook handler).
///
/// Forwards the session-end signal to the daemon so it can remove
/// the session's root contributions from the refcount tracker.
/// Best effort — the host CLI will not wait for completion and
/// ignores all flow-control fields.
pub fn run_session_end() {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let session_id = hook_json.get("session_id").and_then(|v| v.as_str());
    let mut request = serde_json::json!({"method": "session-end/cleanup"});
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }

    // Fire and forget — no response processing needed.
    let _ = ipc_exchange(stream, &request);
}

/// Finalize a [`SystemMessageBuilder`] and print the `systemMessage` JSON
/// if there is content.
fn emit_system_message(builder: crate::hook::response::SystemMessageBuilder, format: HostFormat) {
    if let Some(msg) = builder.finish() {
        print!("{}", format_system_message(&msg, format));
    }
}

/// Format a `systemMessage` for hook responses.
fn format_system_message(msg: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude | HostFormat::Gemini => {
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
    let session_id = hook_json.get("session_id").and_then(|v| v.as_str());

    let mut request = serde_json::json!({
        "method": "post-agent/require-release",
        "agent_id": agent_id,
        "stop_hook_active": stop_hook_active,
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

/// Run diagnostics after reading or editing (`PostToolUse` / `AfterTool` hook handler).
///
/// Reads hook JSON from stdin, finds the session for the file's workspace,
/// connects to the IPC socket, and returns diagnostics for the model's
/// context. Emits `systemMessage` JSON on infrastructure errors so the user
/// sees failures in their terminal.
///
/// For `done_editing`, sends a `post-tool/done-editing` IPC request instead
/// of the per-file `post-tool/diagnostics` — the server drains accumulated
/// files and returns batch diagnostics.
pub fn run_post_tool(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let tool_name = hook_json
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Per-file diagnostics: requires a file path.
    let Some(file_path) = extract_file_path(&hook_json) else {
        print!(
            "{}",
            notify_error(
                "missing file path in hook input — diagnostics skipped",
                format,
            )
        );
        return;
    };

    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let agent_id = extract_agent_id(&hook_json);
    let session_id = hook_json.get("session_id").and_then(|v| v.as_str());

    let mut request = serde_json::json!({
        "method": "post-tool/diagnostics",
        "file": file_path,
        "agent_id": agent_id,
    });
    if !tool_name.is_empty() {
        request["tool"] = serde_json::json!(tool_name);
    }
    if let Some(sid) = session_id {
        request["session_id"] = serde_json::json!(sid);
    }
    request["host_payload"] = prepare_host_payload(&hook_json);

    // Response is unused — the server only accumulates file paths
    // during editing mode. Diagnostics are returned by `done_editing`.
    let _ = ipc_exchange(stream, &request);
}

/// Signal turn start (`UserPromptSubmit` / `BeforeAgent` hook handler).
///
/// Sends a `pre-agent/turn-start` IPC request to the running Catenary session
/// to increment the turn counter. Fires once per user prompt / agent turn.
///
/// Silently succeeds on any error to avoid breaking the host CLI's flow.
pub fn run_pre_agent(format: HostFormat) {
    let _ = format; // Reserved for future per-host output formatting.

    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    if let Some(stream) = hook_connect(&hook_json) {
        let mut request = serde_json::json!({
            "method": "pre-agent/turn-start",
            "host_payload": prepare_host_payload(&hook_json),
        });
        if let Some(tp) = hook_json.get("transcript_path").and_then(|v| v.as_str()) {
            request["transcript_path"] = serde_json::json!(tp);
        }
        if let Some(sid) = hook_json.get("session_id").and_then(|v| v.as_str()) {
            request["session_id"] = serde_json::json!(sid);
        }
        let _ = ipc_exchange(stream, &request);
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

    let tool_name = hook_json
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // ── Command filter ──────────────────────────────────────────
    // Try session-side check first (full multi-root merged config).
    // Fall back to client-side check (user config + cwd's project
    // config) when the session is unreachable.
    if let Some(shell_cmd) = extract_shell_command(&hook_json, tool_name, format) {
        if let Some(reason) = ipc_check_command(&hook_json, &shell_cmd, format) {
            print!("{}", format_deny(&reason, format));
            return;
        }
        // IPC failed or session unreachable — try client-side.
        if let Some((denial, resolved)) = check_shell_command(&hook_json, &shell_cmd) {
            let build_hint = resolve_client_build_hint(&hook_json, &denial.command, &resolved);
            let reason = crate::cli::command_filter::format_denial_full(
                &denial.command,
                &resolved,
                &denial,
                Some(format),
                build_hint.as_deref(),
            );
            print!("{}", format_deny(&reason, format));
            return;
        }
    }

    // ── Editing state enforcement (IPC to daemon / session) ──────
    let Some(stream) = hook_connect(&hook_json) else {
        return;
    };

    let file_path = extract_file_path(&hook_json);
    let agent_id = extract_agent_id(&hook_json);
    let session_id = hook_json.get("session_id").and_then(|v| v.as_str());
    let shell_cmd = extract_shell_command(&hook_json, tool_name, format);

    let mut request = serde_json::json!({
        "method": "pre-tool/editing-state",
        "tool_name": tool_name,
        "agent_id": agent_id,
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
    // Pass the host CLI's cwd for Catenary grep/glob so the session
    // can resolve relative patterns against the agent's working directory.
    if is_catenary_grep_or_glob(tool_name)
        && let Some(c) = hook_json.get("cwd").and_then(|v| v.as_str())
    {
        request["cwd"] = serde_json::json!(c);
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
    let cwd = hook_json
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
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
) -> Option<String> {
    let lookup = denied_cmd.split_whitespace().next().unwrap_or(denied_cmd);
    let crate::config::GuidanceEntry::Build(bg) = resolved.guidance_for(lookup)? else {
        return None;
    };

    let user_config_path = crate::config::config_sources()
        .first()
        .map(|p| p.display().to_string());
    let user_path_str = user_config_path.as_deref().unwrap_or("user config");

    let cwd = hook_json
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
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
fn find_project_config(cwd: &std::path::Path) -> Option<(PathBuf, crate::config::ProjectConfig)> {
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

    let cwd = hook_json.get("cwd").and_then(|v| v.as_str());
    let session_id = hook_json.get("session_id").and_then(|v| v.as_str());

    let format_str = match format {
        HostFormat::Claude => "claude",
        HostFormat::Gemini => "gemini",
    };

    let mut request = serde_json::json!({
        "method": "pre-tool/check-command",
        "command": shell_cmd,
        "format": format_str,
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

/// Returns `true` if the tool name is a Catenary grep or glob tool.
///
/// Matches bare names and MCP-qualified names from Claude Code
/// (`mcp*catenary*grep`) and Gemini CLI (`mcp_catenary_grep`).
fn is_catenary_grep_or_glob(tool_name: &str) -> bool {
    use crate::bridge::is_catenary_tool;
    is_catenary_tool(tool_name, "grep") || is_catenary_tool(tool_name, "glob")
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
    };
    if !is_shell_tool {
        return None;
    }
    let tool_input = hook_json
        .get("tool_input")
        .or_else(|| hook_json.get("args"));
    tool_input
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .map(String::from)
}

// ── Formatting helpers ──────────────────────────────────────────────────

/// GitHub issues URL for user-facing bug report suggestions.
const BUG_REPORT_URL: &str = "https://github.com/TwoWells/Catenary/issues";

/// Format an internal error for the user via `systemMessage`, with a bug
/// report link appended.
///
/// The error is shown to the user in their terminal but not injected into
/// the model's context — the model cannot act on internal Catenary failures.
fn notify_error(message: &str, format: HostFormat) -> String {
    let full =
        format!("Catenary: {message}. If this persists, please file a bug: {BUG_REPORT_URL}");
    format_error(&full, format)
}

/// Format an internal error for the user via `systemMessage`.
///
/// The error is shown to the user in their terminal but not injected into
/// the model's context — the model cannot act on internal Catenary failures.
fn format_error(message: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
            },
            "systemMessage": message
        })
        .to_string(),
        HostFormat::Gemini => serde_json::json!({
            "hookSpecificOutput": {},
            "systemMessage": message
        })
        .to_string(),
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
    fn test_format_error_claude() -> Result<()> {
        let output = format_error("Catenary: database unavailable", HostFormat::Claude);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["systemMessage"], "Catenary: database unavailable");
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(parsed["hookSpecificOutput"]["additionalContext"].is_null());
        Ok(())
    }

    #[test]
    fn test_format_error_gemini() -> Result<()> {
        let output = format_error("Catenary: database unavailable", HostFormat::Gemini);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["systemMessage"], "Catenary: database unavailable");
        assert!(parsed["hookSpecificOutput"]["hookEventName"].is_null());
        assert!(parsed["hookSpecificOutput"]["additionalContext"].is_null());
        Ok(())
    }

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

    // ── is_catenary_grep_or_glob tests ─────────────────────────────────

    #[test]
    fn catenary_grep_or_glob_bare_names() {
        assert!(is_catenary_grep_or_glob("grep"));
        assert!(is_catenary_grep_or_glob("glob"));
    }

    #[test]
    fn catenary_grep_or_glob_claude_code_names() {
        assert!(is_catenary_grep_or_glob(
            "mcp__plugin_catenary_catenary__grep"
        ));
        assert!(is_catenary_grep_or_glob(
            "mcp__plugin_catenary_catenary__glob"
        ));
    }

    #[test]
    fn catenary_grep_or_glob_gemini_names() {
        assert!(is_catenary_grep_or_glob("mcp_catenary_grep"));
        assert!(is_catenary_grep_or_glob("mcp_catenary_glob"));
    }

    #[test]
    fn catenary_grep_or_glob_non_matching() {
        assert!(!is_catenary_grep_or_glob("Edit"));
        assert!(!is_catenary_grep_or_glob("Bash"));
        assert!(!is_catenary_grep_or_glob("start_editing"));
        assert!(!is_catenary_grep_or_glob("mcp_catenary_start_editing"));
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

    // ── is_path_prefix tests ────────────────────────────────────────

    #[test]
    fn path_prefix_exact_match() {
        assert!(is_path_prefix("/home/user/Catenary", "/home/user/Catenary"));
    }

    #[test]
    fn path_prefix_subdirectory() {
        assert!(is_path_prefix(
            "/home/user/Catenary/src",
            "/home/user/Catenary",
        ));
    }

    #[test]
    fn path_prefix_rejects_partial_component() {
        assert!(!is_path_prefix(
            "/home/user/Catenary-06",
            "/home/user/Catenary",
        ));
    }

    #[test]
    fn path_prefix_rejects_partial_component_deep() {
        assert!(!is_path_prefix(
            "/home/user/Catenary-06/src/main.rs",
            "/home/user/Catenary",
        ));
    }

    #[test]
    fn path_prefix_no_match() {
        assert!(!is_path_prefix(
            "/home/user/OtherProject",
            "/home/user/Catenary",
        ));
    }

    // ── find_session_id tests ──────────────────────────────────────────

    /// Insert a test session row directly into the database.
    /// Uses the current process PID so `is_process_alive` returns true.
    fn insert_test_session(
        conn: &rusqlite::Connection,
        id: &str,
        workspace: &str,
        client_session_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, alive, client_session_id) \
             VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z', 1, ?4)",
            rusqlite::params![id, std::process::id(), workspace, client_session_id],
        )
        .expect("insert test session");
    }

    /// Open an isolated test database for hook tests.
    fn hook_test_db() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open test DB");
        (dir, conn)
    }

    #[test]
    fn find_session_by_client_session_id() {
        let (_dir, conn) = hook_test_db();
        insert_test_session(&conn, "cat-001", "/home/user/ProjectA", Some("host-uuid-1"));

        // Hook comes from a different cwd but carries the matching session_id.
        let hook_json = serde_json::json!({
            "session_id": "host-uuid-1",
            "cwd": "/tmp/unrelated"
        });

        let result = find_session_id(&hook_json, &conn);
        assert_eq!(result.as_deref(), Some("cat-001"));
    }

    #[test]
    fn find_session_falls_back_to_cwd_when_no_session_id() {
        let (_dir, conn) = hook_test_db();
        insert_test_session(&conn, "cat-002", "/home/user/ProjectB", Some("host-uuid-2"));

        // No session_id in hook payload — falls back to cwd matching.
        let hook_json = serde_json::json!({
            "cwd": "/home/user/ProjectB/src"
        });

        let result = find_session_id(&hook_json, &conn);
        assert_eq!(result.as_deref(), Some("cat-002"));
    }

    #[test]
    fn find_session_falls_back_to_cwd_when_session_id_unmatched() {
        let (_dir, conn) = hook_test_db();
        insert_test_session(&conn, "cat-003", "/home/user/ProjectC", Some("host-uuid-3"));

        // session_id doesn't match any stored client_session_id,
        // but cwd matches the workspace.
        let hook_json = serde_json::json!({
            "session_id": "unknown-uuid",
            "cwd": "/home/user/ProjectC/tests"
        });

        let result = find_session_id(&hook_json, &conn);
        assert_eq!(result.as_deref(), Some("cat-003"));
    }

    #[test]
    fn find_session_prefers_session_id_over_cwd() {
        let (_dir, conn) = hook_test_db();
        insert_test_session(&conn, "cat-A", "/home/user/Alpha", Some("uuid-alpha"));
        insert_test_session(&conn, "cat-B", "/home/user/Beta", Some("uuid-beta"));

        // cwd matches Alpha, but session_id matches Beta — Beta wins.
        let hook_json = serde_json::json!({
            "session_id": "uuid-beta",
            "cwd": "/home/user/Alpha/src"
        });

        let result = find_session_id(&hook_json, &conn);
        assert_eq!(result.as_deref(), Some("cat-B"));
    }

    #[test]
    fn find_session_returns_none_when_no_match() {
        let (_dir, conn) = hook_test_db();
        insert_test_session(&conn, "cat-X", "/home/user/X", Some("uuid-x"));

        // Neither session_id nor cwd matches.
        let hook_json = serde_json::json!({
            "session_id": "unknown-uuid",
            "cwd": "/tmp/nowhere"
        });

        let result = find_session_id(&hook_json, &conn);
        assert!(result.is_none());
    }
}
