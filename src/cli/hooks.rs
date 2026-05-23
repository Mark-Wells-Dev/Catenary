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
//! - `run_post_agent` — force `done_editing` (`Stop` / `AfterAgent`)
//! - `run_session_start` — clear stale editing state (`SessionStart`)

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use std::path::PathBuf;
use std::time::Duration;

use crate::cli::HostFormat;

/// Connect to the daemon's hook IPC endpoint.
#[cfg(unix)]
fn hook_connect(_hook_json: &serde_json::Value) -> Option<std::os::unix::net::UnixStream> {
    let daemon_path = crate::router::hook_socket_path();
    notify_connect(&daemon_path)
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
        HostFormat::Gemini => serde_json::json!({
            "decision": "deny",
            "reason": reason
        })
        .to_string(),
        HostFormat::Antigravity => serde_json::json!({
            "decision": "deny",
            "message": reason
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
            "decision": "block",
            "message": reason
        })
        .to_string(),
    }
}

/// Extract `session_id` from hook payload, checking host-specific field names.
///
/// Claude Code and Gemini CLI use `session_id`; Antigravity CLI uses
/// `conversationId`.
fn extract_session_id(hook_json: &serde_json::Value) -> Option<&str> {
    hook_json
        .get("session_id")
        .or_else(|| hook_json.get("conversationId"))
        .and_then(|v| v.as_str())
}

/// Extract working directory from hook payload, checking host-specific field names.
///
/// Claude Code and Gemini CLI use `cwd`; Antigravity CLI uses
/// `workspacePaths` (array, first entry used).
fn extract_cwd_str(hook_json: &serde_json::Value) -> Option<&str> {
    hook_json.get("cwd").and_then(|v| v.as_str()).or_else(|| {
        hook_json
            .get("workspacePaths")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
    })
}

/// Extract tool name from hook payload, checking host-specific field names.
///
/// Claude Code and Gemini CLI use `tool_name`; Antigravity CLI uses
/// `toolCall.name`.
fn extract_tool_name(hook_json: &serde_json::Value) -> &str {
    hook_json
        .get("tool_name")
        .and_then(|v| v.as_str())
        .or_else(|| {
            hook_json
                .get("toolCall")
                .and_then(|tc| tc.get("name"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
}

/// Extract `agent_id` from hook payload. Defaults to empty string (main agent).
fn extract_agent_id(hook_json: &serde_json::Value) -> &str {
    hook_json
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// Extracts the file path from hook JSON, checking host-specific field names.
///
/// Claude Code / Gemini CLI: `tool_input.file_path` or `tool_input.file`.
/// Antigravity CLI: `toolCall.args.TargetFile`.
fn extract_file_path(hook_json: &serde_json::Value) -> Option<String> {
    // Claude Code / Gemini CLI
    let file_path = hook_json
        .get("tool_input")
        .and_then(|ti| ti.get("file_path").or_else(|| ti.get("file")))
        .and_then(|fp| fp.as_str())
        // Antigravity CLI
        .or_else(|| {
            hook_json
                .get("toolCall")
                .and_then(|tc| tc.get("args"))
                .and_then(|a| a.get("TargetFile"))
                .and_then(|fp| fp.as_str())
        })?;

    // Resolve to absolute path
    let abs_path = if std::path::Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        let cwd = extract_cwd_str(hook_json).map_or_else(
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

    let session_id = extract_session_id(&hook_json);
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

    let session_id = extract_session_id(&hook_json);
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
    let session_id = extract_session_id(&hook_json);

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
        if let Some(sid) = extract_session_id(&hook_json) {
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

    let tool_name = extract_tool_name(&hook_json);

    // ── Catenary CLI commands ────────────────────────────────────
    // Recognize Catenary subcommands invoked via the host's shell
    // tool. Agent-invocable commands bypass the allowlist. Lifecycle
    // commands (`hook`, `stop`) are unconditionally denied — they
    // are for host CLI hooks and user use only.
    if let Some(ref shell_cmd) = extract_shell_command(&hook_json, tool_name, format) {
        // Unconditional deny: `catenary hook` is for host CLI hook
        // invocation only, `catenary stop` is a user-facing command.
        // Neither should be invoked by an agent.
        if is_catenary_command(shell_cmd, &["hook", "stop"]) {
            print!(
                "{}",
                format_deny(
                    "catenary hook and stop commands are not agent-invocable",
                    format,
                )
            );
            return;
        }
        // start_editing: send IPC to daemon to enter editing mode,
        // then allow the command to execute (it prints confirmation).
        if is_catenary_command(shell_cmd, &["start_editing"]) {
            handle_start_editing_hook(&hook_json, format);
            return;
        }
        // done_editing: send IPC to daemon to prepare the handoff
        // (drain files, release guardrail, deposit in handoff slot),
        // then allow the command to execute (it retrieves diagnostics).
        if is_catenary_command(shell_cmd, &["done_editing"]) {
            handle_done_editing_hook(&hook_json, format);
            return;
        }
        // Root management: allow through without IPC — the CLI
        // command handles root management via daemon IPC directly.
        if is_root_command(shell_cmd) {
            return;
        }
    }

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
    let session_id = extract_session_id(&hook_json);
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
    let cwd = extract_cwd_str(hook_json).map(PathBuf::from);
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

    let cwd = extract_cwd_str(hook_json).map(PathBuf::from);
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

    let cwd = extract_cwd_str(hook_json);
    let session_id = extract_session_id(hook_json);

    let format_str = match format {
        HostFormat::Claude => "claude",
        HostFormat::Gemini => "gemini",
        HostFormat::Antigravity => "antigravity",
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
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .map(String::from)
}

/// Returns `true` if the shell command is a standalone `catenary`
/// invocation with one of the given subcommands.
///
/// Matches the command after trimming, with or without a path prefix
/// (e.g., `/usr/local/bin/catenary`). Does **not** match compound
/// commands (`foo && catenary start_editing`) — the command must be
/// a single invocation.
///
/// The subcommand match is prefix-based: `catenary add-root /tmp/foo`
/// matches subcommand `"add-root"`.
fn is_catenary_command(shell_cmd: &str, subcommands: &[&str]) -> bool {
    let trimmed = shell_cmd.trim();

    // Reject compound commands: any shell operator means the catenary
    // invocation is embedded in a larger command. Use the command
    // filter's tokenizer to check — it handles quoting correctly.
    let names = crate::cli::command_filter::extract_command_names(trimmed);
    if names.len() != 1 {
        return false;
    }

    // The single command name must be "catenary".
    if names[0] != "catenary" {
        return false;
    }

    // Check if the rest of the command contains a matching subcommand.
    // Find "catenary" in the trimmed string, then check the next token.
    let basename_pos = trimmed.rfind("catenary").map(|p| p + "catenary".len());
    let Some(after_catenary) = basename_pos.map(|p| trimmed[p..].trim_start()) else {
        return false;
    };

    subcommands
        .iter()
        .any(|sub| after_catenary == *sub || after_catenary.starts_with(&format!("{sub} ")))
}

/// Returns `true` if the shell command is a `catenary add-root` or
/// `catenary rm-root` invocation.
///
/// These are Catenary's own management commands — they bypass the
/// command filter allowlist. The CLI command handles root management
/// directly via daemon IPC.
fn is_root_command(shell_cmd: &str) -> bool {
    is_catenary_command(shell_cmd, &["add-root", "rm-root"])
}

// ── Catenary CLI command hooks ──────────────────────────────────────────

/// Handle `PreToolUse` for `catenary start_editing`.
///
/// Sends `pre-tool/start-editing` IPC to the daemon to enter editing mode.
/// The daemon checks the editing guardrail. Returns allow (silent) or deny
/// (prints denial reason to stdout for the host CLI).
fn handle_start_editing_hook(hook_json: &serde_json::Value, format: HostFormat) {
    let Some(stream) = hook_connect(hook_json) else {
        return;
    };

    let agent_id = extract_agent_id(hook_json);
    let session_id = extract_session_id(hook_json);

    let mut request = serde_json::json!({
        "method": "pre-tool/start-editing",
        "agent_id": agent_id,
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

/// Handle `PreToolUse` for `catenary done_editing`.
///
/// Sends `pre-tool/done-editing-prepare` IPC to the daemon to prepare
/// the handoff: drain accumulated files, release the editing guardrail,
/// and deposit the file list in the handoff slot. Returns allow (silent)
/// or deny (prints denial reason to stdout for the host CLI).
fn handle_done_editing_hook(hook_json: &serde_json::Value, format: HostFormat) {
    let Some(stream) = hook_connect(hook_json) else {
        return;
    };

    let agent_id = extract_agent_id(hook_json);
    let session_id = extract_session_id(hook_json);

    let mut request = serde_json::json!({
        "method": "pre-tool/done-editing",
        "agent_id": agent_id,
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
            extract_file_path(&json),
            Some("/home/user/project/src/main.rs".to_string()),
        );
    }

    #[test]
    fn extract_file_path_file_key() {
        let json = serde_json::json!({
            "tool_input": { "file": "/tmp/test.py" }
        });
        assert_eq!(extract_file_path(&json), Some("/tmp/test.py".to_string()),);
    }

    #[test]
    fn extract_file_path_relative_with_cwd() {
        let json = serde_json::json!({
            "cwd": "/home/user/project",
            "tool_input": { "file_path": "src/main.rs" }
        });
        let result = extract_file_path(&json).expect("should resolve relative path");
        assert_eq!(result, "/home/user/project/src/main.rs");
    }

    #[test]
    fn extract_file_path_missing_tool_input() {
        let json = serde_json::json!({});
        assert!(extract_file_path(&json).is_none());
    }

    #[test]
    fn extract_file_path_missing_file_keys() {
        let json = serde_json::json!({
            "tool_input": { "command": "ls" }
        });
        assert!(extract_file_path(&json).is_none());
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
            extract_file_path(&json),
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

    // ── is_catenary_command tests ───────────────────────────────────

    #[test]
    fn catenary_command_start_editing() {
        assert!(is_catenary_command(
            "catenary start_editing",
            &["start_editing"]
        ));
    }

    #[test]
    fn catenary_command_with_path_prefix() {
        assert!(is_catenary_command(
            "/usr/local/bin/catenary start_editing",
            &["start_editing"],
        ));
    }

    #[test]
    fn catenary_command_with_home_path_prefix() {
        assert!(is_catenary_command(
            "/home/user/.local/bin/catenary start_editing",
            &["start_editing"],
        ));
    }

    #[test]
    fn catenary_command_relative_path_prefix() {
        assert!(is_catenary_command(
            "./catenary start_editing",
            &["start_editing"],
        ));
    }

    #[test]
    fn catenary_command_with_whitespace() {
        assert!(is_catenary_command(
            "  catenary start_editing  ",
            &["start_editing"],
        ));
    }

    #[test]
    fn catenary_command_compound_not_matched() {
        assert!(!is_catenary_command(
            "echo hello && catenary start_editing",
            &["start_editing"],
        ));
    }

    #[test]
    fn catenary_command_pipe_not_matched() {
        assert!(!is_catenary_command(
            "echo foo | catenary start_editing",
            &["start_editing"],
        ));
    }

    #[test]
    fn catenary_command_semicolon_not_matched() {
        assert!(!is_catenary_command(
            "echo foo; catenary start_editing",
            &["start_editing"],
        ));
    }

    #[test]
    fn catenary_command_wrong_subcommand() {
        assert!(!is_catenary_command("catenary doctor", &["start_editing"]));
    }

    #[test]
    fn catenary_command_not_catenary() {
        assert!(!is_catenary_command("ls -la", &["start_editing"]));
    }

    #[test]
    fn catenary_command_multiple_subcommands() {
        assert!(is_catenary_command(
            "catenary add-root /tmp/project",
            &["add-root", "rm-root"],
        ));
        assert!(is_catenary_command(
            "catenary rm-root /tmp/project",
            &["add-root", "rm-root"],
        ));
        assert!(!is_catenary_command(
            "catenary doctor",
            &["add-root", "rm-root"],
        ));
    }

    // ── is_root_command tests ─────────────────────────────────────

    #[test]
    fn is_root_command_add_root() {
        assert!(is_root_command("catenary add-root /tmp/project"));
    }

    #[test]
    fn is_root_command_rm_root() {
        assert!(is_root_command("catenary rm-root /tmp/project"));
    }

    #[test]
    fn is_root_command_with_whitespace() {
        assert!(is_root_command("  catenary add-root /tmp/root  "));
    }

    #[test]
    fn is_root_command_with_path_prefix() {
        assert!(is_root_command(
            "/usr/local/bin/catenary add-root /tmp/root"
        ));
    }

    #[test]
    fn is_root_command_non_matching() {
        assert!(!is_root_command("catenary doctor"));
        assert!(!is_root_command("ls -la"));
        assert!(!is_root_command("catenary stop"));
        assert!(!is_root_command("catenary start_editing"));
    }

    #[test]
    fn is_root_command_compound_not_matched() {
        assert!(!is_root_command("echo hello && catenary add-root /tmp"));
    }

    // ── done_editing command recognition tests ──────────────────────

    #[test]
    fn catenary_command_done_editing() {
        assert!(is_catenary_command(
            "catenary done_editing",
            &["done_editing"],
        ));
    }

    #[test]
    fn catenary_command_done_editing_with_path_prefix() {
        assert!(is_catenary_command(
            "/usr/local/bin/catenary done_editing",
            &["done_editing"],
        ));
    }

    #[test]
    fn catenary_command_done_editing_with_home_prefix() {
        assert!(is_catenary_command(
            "~/.local/bin/catenary done_editing",
            &["done_editing"],
        ));
    }

    #[test]
    fn catenary_command_done_editing_compound_not_matched() {
        assert!(!is_catenary_command(
            "echo hello && catenary done_editing",
            &["done_editing"],
        ));
    }

    // ── unconditional deny tests ────────────────────────────────────

    #[test]
    fn catenary_hook_subcommand_detected() {
        assert!(is_catenary_command(
            "catenary hook pre-tool --format=claude",
            &["hook", "stop"],
        ));
    }

    #[test]
    fn catenary_stop_subcommand_detected() {
        assert!(is_catenary_command("catenary stop", &["hook", "stop"]));
    }

    #[test]
    fn catenary_hook_with_path_prefix_detected() {
        assert!(is_catenary_command(
            "/usr/local/bin/catenary hook post-tool --format=gemini",
            &["hook", "stop"],
        ));
    }

    #[test]
    fn catenary_stop_not_matched_as_start_editing() {
        assert!(!is_catenary_command("catenary stop", &["start_editing"]));
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
        assert_eq!(extract_session_id(&json), Some("conv-abc-123"));
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
        assert_eq!(extract_file_path(&json), Some("/src/main.rs".to_string()),);
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
            extract_file_path(&json),
            Some("/home/user/project/src/main.rs".to_string()),
        );
    }

    #[test]
    fn antigravity_deny_response_format() -> Result<()> {
        let output = format_deny(
            "run `catenary start_editing` before editing",
            HostFormat::Antigravity,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["decision"], "deny");
        assert_eq!(
            parsed["message"],
            "run `catenary start_editing` before editing",
        );
        // Antigravity format should NOT have hookSpecificOutput
        assert!(parsed["hookSpecificOutput"].is_null());
        // Antigravity uses "message" not "reason"
        assert!(parsed["reason"].is_null());
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
                "args": { "command": "catenary start_editing" }
            }
        });
        let tool_name = extract_tool_name(&json);
        assert_eq!(tool_name, "run_command");
        assert_eq!(
            extract_shell_command(&json, tool_name, HostFormat::Antigravity),
            Some("catenary start_editing".to_string()),
        );
    }

    #[test]
    fn antigravity_stop_block_format() -> Result<()> {
        let output = format_stop_block("files still in editing state", HostFormat::Antigravity);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        assert_eq!(parsed["decision"], "block");
        assert_eq!(parsed["message"], "files still in editing state");
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
        assert_eq!(extract_session_id(&json), Some("claude-sess-1"));
    }

    #[test]
    fn extract_session_id_antigravity() {
        let json = serde_json::json!({ "conversationId": "ag-conv-1" });
        assert_eq!(extract_session_id(&json), Some("ag-conv-1"));
    }

    #[test]
    fn extract_session_id_missing() {
        let json = serde_json::json!({});
        assert!(extract_session_id(&json).is_none());
    }

    // ── extract_cwd_str tests ─────────────────────────────────────────

    #[test]
    fn extract_cwd_str_claude() {
        let json = serde_json::json!({ "cwd": "/home/user/project" });
        assert_eq!(extract_cwd_str(&json), Some("/home/user/project"));
    }

    #[test]
    fn extract_cwd_str_antigravity() {
        let json = serde_json::json!({ "workspacePaths": ["/home/user/project", "/other"] });
        assert_eq!(extract_cwd_str(&json), Some("/home/user/project"));
    }

    #[test]
    fn extract_cwd_str_missing() {
        let json = serde_json::json!({});
        assert!(extract_cwd_str(&json).is_none());
    }

    // ── extract_tool_name tests ───────────────────────────────────────

    #[test]
    fn extract_tool_name_claude() {
        let json = serde_json::json!({ "tool_name": "Edit" });
        assert_eq!(extract_tool_name(&json), "Edit");
    }

    #[test]
    fn extract_tool_name_antigravity() {
        let json = serde_json::json!({
            "toolCall": { "name": "write_to_file" }
        });
        assert_eq!(extract_tool_name(&json), "write_to_file");
    }

    #[test]
    fn extract_tool_name_missing() {
        let json = serde_json::json!({});
        assert_eq!(extract_tool_name(&json), "");
    }

    // ── Antigravity shell command tests ────────────────────────────────

    #[test]
    fn extract_shell_command_antigravity_run_command() {
        let json = serde_json::json!({
            "toolCall": {
                "name": "run_command",
                "args": { "command": "make test" }
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
}
