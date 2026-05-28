// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! IPC server for host CLI hook integration.
//!
//! `HookServer` is the protocol boundary for all hook traffic, same as
//! `McpServer` is for MCP and `Connection`/`LspServer` is for LSP. All
//! hook logic runs server-side. CLI hook processes are dumb transports:
//! read stdin from the host CLI, connect to IPC, forward the request,
//! format the response for the host.
//!
//! Hook methods are caller-supplied and follow the `namespace/action`
//! convention used by MCP (`tools/call`) and LSP (`textDocument/hover`).
//!
//! Transport: Unix domain sockets on Unix, named pipes on Windows.

pub mod response;

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tracing::{debug, info};

use crate::bridge::HookRouter;
use crate::bridge::session::Session;
use crate::protocol::category::hook_category;

/// Emit a hook protocol event at the given tracing level.
///
/// Protocol routing is by `kind` field — `MessageDbSink` matches
/// `kind in {lsp, mcp, hook}` regardless of tracing level.
/// The level controls DB `level` column and TUI filtering threshold.
pub(crate) fn emit_hook_event(
    level: tracing::Level,
    client_name: &str,
    method: &str,
    parent_id: Option<&str>,
    payload: &str,
    msg: &str,
) {
    if level == tracing::Level::ERROR {
        crate::emit_protocol_event!(
            error,
            kind = "hook",
            method = method,
            server = "catenary",
            client = client_name,
            scope_root = "",
            parent_id = parent_id,
            payload = payload,
            "{msg}"
        );
    } else if level == tracing::Level::WARN {
        crate::emit_protocol_event!(
            warn,
            kind = "hook",
            method = method,
            server = "catenary",
            client = client_name,
            scope_root = "",
            parent_id = parent_id,
            payload = payload,
            "{msg}"
        );
    } else if level == tracing::Level::INFO {
        crate::emit_protocol_event!(
            info,
            kind = "hook",
            method = method,
            server = "catenary",
            client = client_name,
            scope_root = "",
            parent_id = parent_id,
            payload = payload,
            "{msg}"
        );
    } else {
        crate::emit_protocol_event!(
            debug,
            kind = "hook",
            method = method,
            server = "catenary",
            client = client_name,
            scope_root = "",
            parent_id = parent_id,
            payload = payload,
            "{msg}"
        );
    }
}

/// IPC request from the CLI hook process to the hook server.
///
/// Dispatched by the `method` field. Each variant corresponds to one of the
/// five host CLI hooks.
#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub(crate) enum HookRequest {
    /// Turn boundary signal (fires at each user prompt / agent turn start).
    #[serde(rename = "pre-agent/turn-start")]
    PreAgent {
        /// Host CLI session ID. Used by the daemon to route the hook
        /// to the correct per-session `HookRouter`.
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Editing state enforcement: deny or allow a tool call.
    #[serde(rename = "pre-tool/editing-state")]
    PreTool {
        /// Host CLI tool name (e.g., "Edit", "Write", `"write_file"`).
        tool_name: String,
        /// Absolute path to the target file. Used for scope boundary
        /// checks — edits on files outside workspace roots skip the
        /// `start_editing` gate.
        #[serde(default)]
        file_path: Option<String>,
        /// Shell command string for Bash/`run_shell_command` tools.
        /// Used during editing mode to allow filesystem-only commands
        /// (`rm`, `cp`, `mv`, etc.) without requiring `done_editing`.
        #[serde(default)]
        command: Option<String>,
        /// Agent ID (empty string for the main agent).
        #[serde(default)]
        agent_id: String,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Enter editing mode via CLI command (`catenary editing start`).
    ///
    /// Sent by the `PreToolUse` hook when the agent runs
    /// `catenary editing start` via the host's shell tool. The daemon
    /// enters editing mode for the session.
    #[serde(rename = "pre-tool/editing-start")]
    PreToolStartEditing {
        /// Agent ID (empty string for the main agent).
        #[serde(default)]
        agent_id: String,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Session-side command check with debounce.
    ///
    /// Evaluates the shell command against the merged allowlist (user
    /// config + all project configs for current roots). On denial,
    /// applies turn-based debounce: first denial in a turn returns
    /// the full config dump, subsequent denials return a short message.
    #[serde(rename = "pre-tool/check-command")]
    CheckCommand {
        /// The shell command string to evaluate.
        command: String,
        /// Working directory from the hook payload (for per-root build lookup).
        #[serde(default)]
        cwd: Option<String>,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
        /// Host CLI format (`"claude"` or `"gemini"`), for per-client
        /// template variable resolution in guidance messages.
        #[serde(default)]
        format: Option<String>,
    },

    /// Force `editing stop` before the agent stops.
    #[serde(rename = "post-agent/require-release")]
    PostAgent {
        /// Agent ID (empty string for the main agent).
        #[serde(default)]
        agent_id: String,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
        /// Whether this is a retry (Claude Code `stop_hook_active`).
        #[serde(default)]
        stop_hook_active: bool,
    },

    /// Prepare handoff for `catenary editing stop` CLI command.
    ///
    /// Sent by the `PreToolUse` hook when the agent runs
    /// `catenary editing stop` via the host's shell tool. The daemon
    /// acquires the handoff lock, drains accumulated files, releases
    /// the editing guardrail, and deposits the file list in the
    /// handoff slot for the subsequent `done-editing/run` request.
    #[serde(rename = "pre-tool/editing-stop")]
    PreToolDoneEditingPrepare {
        /// Agent ID (empty string for the main agent).
        /// Deserialized from IPC but not consumed — the daemon
        /// handles preparation at the dispatch level.
        #[serde(default)]
        #[allow(
            dead_code,
            reason = "deserialized from IPC protocol, consumed by serde"
        )]
        agent_id: String,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Execute the `editing stop` pipeline and return diagnostics.
    ///
    /// Sent by the `catenary editing stop` CLI command after the
    /// `PreToolUse` hook has prepared the handoff slot. Takes the file
    /// list from the slot, runs `process_files_batched`, and returns
    /// formatted diagnostics.
    #[serde(rename = "tool/editing-stop")]
    DoneEditingRun,

    /// Clear stale editing state on session start.
    #[serde(rename = "session-start/clear-editing")]
    SessionStart {
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Clean up session state on session end.
    ///
    /// Fires when the host CLI session ends (exit, `/clear`, resume,
    /// logout). No decision control — the session is already ending.
    /// Used by the daemon to remove the session's root contributions
    /// from the refcount tracker.
    #[serde(rename = "session-end/cleanup")]
    SessionEnd {
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },
}

/// IPC response from the hook server to the CLI.
///
/// Handlers return `Option<HookResult>`: `None` means "allow" (empty
/// response — CLI outputs nothing). Variants carry actionable data
/// for the CLI to format for the host.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookResult {
    /// Deny with reason (pre-tool enforcement).
    Deny(String),
    /// Block with reason (post-agent enforcement).
    Block(String),
    /// Cleared editing state entries.
    Cleared(usize),
}

/// IPC response envelope carrying both the handler result and an optional
/// `systemMessage` for the user.
///
/// The notification queue is drained at stationary hook points (`SessionStart`,
/// `Stop`/`AfterAgent` when allowing) and delivered as `system_message`. The CLI
/// hook process embeds this string in the host-specific `systemMessage` JSON
/// field.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, PartialEq, Eq)]
pub struct HookResponseEnvelope {
    /// Handler result (`None` = allow / no actionable data).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<HookResult>,
    /// Composed `systemMessage` content from direct messages and background
    /// notification drain. `None` = no `systemMessage` field in host output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
}

// ── HookServer ──────────────────────────────────────────────────────────

/// Listens on an IPC endpoint for hook requests from the host CLI.
///
/// Protocol boundary for all hook traffic. Parses IPC messages, logs
/// request/response pairs for monitor visibility, and delegates application
/// dispatch to [`HookRouter`].
pub struct HookServer {
    router: Arc<HookRouter>,
}

impl HookServer {
    /// Creates a new `HookServer`.
    #[must_use]
    pub fn new(
        session: Arc<Session>,
        conn: Arc<Mutex<Connection>>,
        instance_id: Arc<str>,
        client_name: String,
    ) -> Self {
        let router = Arc::new(HookRouter::new(session, conn, instance_id, client_name));
        Self { router }
    }

    /// Starts listening on the given IPC endpoint.
    ///
    /// Spawns a background task that accepts connections and processes
    /// hook requests. Returns a `JoinHandle` for the listener task.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint cannot be created.
    #[cfg(unix)]
    pub fn start(self, socket_path: &std::path::Path) -> Result<tokio::task::JoinHandle<()>> {
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path).map_err(|e| {
            anyhow!(
                "Failed to bind notify socket {}: {e}",
                socket_path.display()
            )
        })?;

        info!("Notify socket listening on {}", socket_path.display());

        let server = Arc::new(self);

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server.handle_connection(stream).await {
                                debug!("Hook IPC connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        debug!("Hook IPC accept error: {e}");
                    }
                }
            }
        });

        Ok(handle)
    }

    /// Starts listening on the given named pipe path.
    ///
    /// Spawns a background task that accepts connections and processes
    /// hook requests. Returns a `JoinHandle` for the listener task.
    ///
    /// # Errors
    ///
    /// Returns an error if the named pipe cannot be created.
    #[cfg(windows)]
    pub fn start(self, pipe_path: &std::path::Path) -> Result<tokio::task::JoinHandle<()>> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let pipe_name = pipe_path.to_string_lossy().to_string();

        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .map_err(|e| anyhow!("Failed to create notify pipe {pipe_name}: {e}"))?;

        info!("Notify pipe listening on {pipe_name}");

        let server_arc = Arc::new(self);

        let handle = tokio::spawn(async move {
            loop {
                // Wait for a client to connect to the current instance
                if let Err(e) = server.connect().await {
                    debug!("Notify pipe connect error: {e}");
                    continue;
                }

                let connected = server;

                // Create a fresh pipe instance before spawning the handler
                // so clients never see ERROR_FILE_NOT_FOUND
                server = match ServerOptions::new().create(&pipe_name) {
                    Ok(s) => s,
                    Err(e) => {
                        info!("Notify pipe create error: {e}");
                        break;
                    }
                };

                let srv = server_arc.clone();
                tokio::spawn(async move {
                    if let Err(e) = srv.handle_connection(connected).await {
                        debug!("Hook IPC connection error: {e}");
                    }
                });
            }
        });

        Ok(handle)
    }

    /// Handles a single connection: reads a JSON request, extracts the method
    /// string, dispatches to the appropriate handler, logs both request and
    /// response at the outcome-determined level, and writes back the result.
    ///
    /// Request logging is deferred until after dispatch so that both the
    /// request and response are emitted at the same level. This prevents
    /// asymmetric levels from breaking pair merge in the TUI.
    async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(&self, stream: S) -> Result<()> {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;

        let raw: Value =
            serde_json::from_str(line.trim()).map_err(|e| anyhow!("Invalid request: {e}"))?;
        let method = raw
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Mint a UUID for this request/response pair
        let scope_id = uuid::Uuid::new_v4().to_string();

        let request: HookRequest = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("Invalid hook request: {e}"))?;

        let result = self.router.dispatch(request);

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
        // Hook allows (empty response) → debug, hook blocks/diagnostics → info.
        let level = Self::hook_outcome_level(&method, &envelope);

        // Log incoming hook request (deferred — uses outcome-determined level)
        emit_hook_event(
            level,
            &self.router.client_name,
            &method,
            Some(&scope_id),
            &raw.to_string(),
            "incoming hook",
        );

        // Log outgoing hook response — same parent_id as request
        emit_hook_event(
            level,
            &self.router.client_name,
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

    /// Determine the tracing level for a hook request/response pair
    /// based on the method category and the dispatch outcome.
    fn hook_outcome_level(method: &str, envelope: &HookResponseEnvelope) -> tracing::Level {
        crate::hook::hook_outcome_level(method, envelope)
    }
}

/// Determine the tracing level for a hook request/response pair
/// based on the method category and the dispatch outcome.
///
/// Used by both [`HookServer`] (per-session IPC) and the daemon's
/// hook dispatch path in [`crate::router::SessionManager`].
pub(crate) fn hook_outcome_level(method: &str, envelope: &HookResponseEnvelope) -> tracing::Level {
    let category = hook_category(method);
    match category {
        // lifecycle: non-empty result → info, empty → debug
        "lifecycle" => {
            if envelope.result.is_some() {
                tracing::Level::INFO
            } else {
                tracing::Level::DEBUG
            }
        }
        // unknown and everything else → debug
        _ => tracing::Level::DEBUG,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Serialization tests ─────────────────────────────────────────────

    #[test]
    fn hook_result_deny_round_trip() {
        let original = HookResult::Deny("call editing start first".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn hook_result_block_round_trip() {
        let original = HookResult::Block("call editing stop first".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn hook_result_cleared_round_trip() {
        let original = HookResult::Cleared(3);
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    // ── Request deserialization tests ────────────────────────────────────

    #[test]
    #[allow(clippy::too_many_lines, reason = "one assertion per variant")]
    fn test_hook_request_tagged_deserialization() {
        // pre-agent/turn-start (no session_id)
        let json = r#"{"method": "pre-agent/turn-start"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("turn-start");
        assert!(matches!(req, HookRequest::PreAgent { session_id: None }));

        // pre-agent/turn-start with session_id
        let json = r#"{"method": "pre-agent/turn-start", "session_id": "sess-123"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("turn-start with session_id");
        assert!(matches!(
            req,
            HookRequest::PreAgent {
                session_id: Some(ref s),
            } if s == "sess-123"
        ));

        // pre-tool/editing-state with all fields
        let json = r#"{"method": "pre-tool/editing-state", "tool_name": "Edit", "file_path": "/tmp/foo.rs", "agent_id": "", "session_id": "abc123"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("editing-state");
        let HookRequest::PreTool {
            tool_name,
            file_path,
            command,
            agent_id,
            session_id,
        } = req
        else {
            unreachable!("expected PreTool");
        };
        assert_eq!(tool_name, "Edit");
        assert_eq!(file_path.as_deref(), Some("/tmp/foo.rs"));
        assert!(command.is_none());
        assert_eq!(agent_id, "");
        assert_eq!(session_id.as_deref(), Some("abc123"));

        // pre-tool/editing-state with command (Bash tool)
        let json = r#"{"method": "pre-tool/editing-state", "tool_name": "Bash", "command": "rm -rf target/", "agent_id": ""}"#;
        let req: HookRequest = serde_json::from_str(json).expect("editing-state with command");
        let HookRequest::PreTool { command, .. } = req else {
            unreachable!("expected PreTool");
        };
        assert_eq!(command.as_deref(), Some("rm -rf target/"));

        // post-agent/require-release (without session_id — backward compat)
        let json =
            r#"{"method": "post-agent/require-release", "agent_id": "", "stop_hook_active": true}"#;
        let req: HookRequest = serde_json::from_str(json).expect("require-release");
        let HookRequest::PostAgent {
            session_id,
            stop_hook_active,
            ..
        } = req
        else {
            unreachable!("expected PostAgent");
        };
        assert!(stop_hook_active);
        assert!(session_id.is_none());

        // post-agent/require-release with session_id
        let json = r#"{"method": "post-agent/require-release", "agent_id": "sub-1", "session_id": "sess-abc", "stop_hook_active": false}"#;
        let req: HookRequest = serde_json::from_str(json).expect("require-release with session");
        let HookRequest::PostAgent {
            agent_id,
            session_id,
            stop_hook_active,
        } = req
        else {
            unreachable!("expected PostAgent");
        };
        assert_eq!(agent_id, "sub-1");
        assert_eq!(session_id.as_deref(), Some("sess-abc"));
        assert!(!stop_hook_active);

        // session-start/clear-editing
        let json = r#"{"method": "session-start/clear-editing", "session_id": "uuid-123"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("clear-editing");
        let HookRequest::SessionStart { session_id } = req else {
            unreachable!("expected SessionStart");
        };
        assert_eq!(session_id.as_deref(), Some("uuid-123"));

        // pre-tool/editing-start
        let json = r#"{"method": "pre-tool/editing-start", "agent_id": "sub-1", "session_id": "sess-xyz"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("editing-start");
        let HookRequest::PreToolStartEditing {
            agent_id,
            session_id,
        } = req
        else {
            unreachable!("expected PreToolStartEditing");
        };
        assert_eq!(agent_id, "sub-1");
        assert_eq!(session_id.as_deref(), Some("sess-xyz"));

        // pre-tool/editing-start minimal (defaults)
        let json = r#"{"method": "pre-tool/editing-start"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("editing-start minimal");
        assert!(matches!(
            req,
            HookRequest::PreToolStartEditing { agent_id, session_id: None } if agent_id.is_empty()
        ));

        // pre-tool/editing-stop
        let json =
            r#"{"method": "pre-tool/editing-stop", "agent_id": "sub-1", "session_id": "sess-abc"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("pre-tool/editing-stop");
        let HookRequest::PreToolDoneEditingPrepare { session_id, .. } = req else {
            unreachable!("expected PreToolDoneEditingPrepare");
        };
        assert_eq!(session_id.as_deref(), Some("sess-abc"));

        // pre-tool/editing-stop minimal (defaults)
        let json = r#"{"method": "pre-tool/editing-stop"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("pre-tool/editing-stop minimal");
        assert!(matches!(
            req,
            HookRequest::PreToolDoneEditingPrepare {
                session_id: None,
                ..
            }
        ));

        // tool/editing-stop
        let json = r#"{"method": "tool/editing-stop"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("tool/editing-stop");
        assert!(matches!(req, HookRequest::DoneEditingRun));

        // pre-tool/check-command
        let json = r#"{"method": "pre-tool/check-command", "command": "cargo test", "cwd": "/project", "session_id": "abc123", "format": "claude"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("check-command");
        let HookRequest::CheckCommand {
            command,
            cwd,
            session_id,
            format,
        } = req
        else {
            unreachable!("expected CheckCommand");
        };
        assert_eq!(command, "cargo test");
        assert_eq!(cwd.as_deref(), Some("/project"));
        assert_eq!(session_id.as_deref(), Some("abc123"));
        assert_eq!(format.as_deref(), Some("claude"));

        // pre-tool/check-command minimal (only command required)
        let json = r#"{"method": "pre-tool/check-command", "command": "ls"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("check-command minimal");
        assert!(matches!(
            req,
            HookRequest::CheckCommand { command, cwd: None, session_id: None, format: None } if command == "ls"
        ));

        // session-end/cleanup
        let json = r#"{"method": "session-end/cleanup", "session_id": "uuid-456"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("session-end");
        let HookRequest::SessionEnd { session_id } = req else {
            unreachable!("expected SessionEnd");
        };
        assert_eq!(session_id.as_deref(), Some("uuid-456"));

        // session-end/cleanup minimal (no session_id)
        let json = r#"{"method": "session-end/cleanup"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("session-end minimal");
        assert!(matches!(req, HookRequest::SessionEnd { session_id: None }));
    }

    // ── Logging tests ───────────────────────────────────────────────────

    use crate::logging::test_support::{MsgRow, query_all_messages, setup_logging};

    /// Filter to hook protocol rows only.
    fn hook_messages(conn: &Arc<std::sync::Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
        query_all_messages(conn)
            .into_iter()
            .filter(|m| m.r#type == "hook")
            .collect()
    }

    #[test]
    fn hook_request_writes_protocol_row() {
        let (_logging, conn, _guard) = setup_logging();

        emit_hook_event(
            tracing::Level::INFO,
            "claude-code",
            "pre-tool/editing-state",
            Some("scope-1"),
            &serde_json::json!({
                "method": "pre-tool/editing-state",
                "file": "/tmp/test.rs",
                "tool": "Write"
            })
            .to_string(),
            "incoming hook",
        );

        let rows = hook_messages(&conn);
        assert!(!rows.is_empty(), "should have at least the hook row");
        assert_eq!(rows[0].method, "pre-tool/editing-state");
        assert_eq!(rows[0].client, "claude-code");
    }

    #[test]
    fn hook_pair_merges() {
        let (_logging, conn, _guard) = setup_logging();

        let scope_id = "scope-pair-test";

        // Incoming request
        emit_hook_event(
            tracing::Level::INFO,
            "claude-code",
            "pre-tool/editing-state",
            Some(scope_id),
            &serde_json::json!({
                "method": "pre-tool/editing-state",
                "file": "/tmp/test.rs"
            })
            .to_string(),
            "incoming hook",
        );

        // Outgoing response — same parent_id
        emit_hook_event(
            tracing::Level::INFO,
            "claude-code",
            "pre-tool/editing-state",
            Some(scope_id),
            &serde_json::json!({"content": "[clean]"}).to_string(),
            "outgoing hook response",
        );

        let rows = hook_messages(&conn);
        assert!(
            rows.len() >= 2,
            "should have at least request + response, got {}",
            rows.len()
        );
        // Both share the same parent_id
        assert_eq!(rows[0].parent_id.as_deref(), Some(scope_id));
        assert_eq!(rows[1].parent_id.as_deref(), Some(scope_id));
    }

    #[test]
    fn hook_turn_start_writes_protocol_row() {
        let (_logging, conn, _guard) = setup_logging();

        let scope_id = "turn-scope";
        emit_hook_event(
            tracing::Level::INFO,
            "host",
            "pre-agent/turn-start",
            Some(scope_id),
            &serde_json::json!({"method": "pre-agent/turn-start"}).to_string(),
            "incoming hook",
        );

        emit_hook_event(
            tracing::Level::INFO,
            "host",
            "pre-agent/turn-start",
            Some(scope_id),
            "",
            "outgoing hook response",
        );

        let rows = hook_messages(&conn);
        assert!(
            rows.len() >= 2,
            "should have at least request + response, got {}",
            rows.len()
        );
        assert_eq!(rows[0].method, "pre-agent/turn-start");
        assert_eq!(rows[0].client, "host");
    }

    // ── Level-aware emit tests ──────────────────────────────────────

    #[test]
    fn emit_at_debug_writes_debug_level() {
        let (_logging, conn, _guard) = setup_logging();

        emit_hook_event(
            tracing::Level::DEBUG,
            "test",
            "pre-tool/editing-state",
            None,
            "{}",
            "debug emit",
        );

        let rows = hook_messages(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, "debug");
    }

    #[test]
    fn emit_at_info_writes_info_level() {
        let (_logging, conn, _guard) = setup_logging();

        emit_hook_event(
            tracing::Level::INFO,
            "test",
            "pre-tool/editing-state",
            None,
            "{}",
            "info emit",
        );

        let rows = hook_messages(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, "info");
    }

    #[test]
    fn emit_at_warn_writes_warn_level() {
        let (_logging, conn, _guard) = setup_logging();

        emit_hook_event(
            tracing::Level::WARN,
            "test",
            "pre-tool/editing-state",
            None,
            "{}",
            "warn emit",
        );

        let rows = hook_messages(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, "warn");
    }

    #[test]
    fn emit_at_error_writes_error_level() {
        let (_logging, conn, _guard) = setup_logging();

        emit_hook_event(
            tracing::Level::ERROR,
            "test",
            "pre-tool/editing-state",
            None,
            "{}",
            "error emit",
        );

        let rows = hook_messages(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, "error");
    }

    // ── Envelope serialization tests ──────────────────────────────────

    #[test]
    fn envelope_result_only() {
        let env = HookResponseEnvelope {
            result: Some(HookResult::Deny("call editing start first".into())),
            system_message: None,
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: HookResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            parsed.result,
            Some(HookResult::Deny("call editing start first".into()))
        );
        assert!(parsed.system_message.is_none());
        // system_message should be absent from JSON (skip_serializing_if)
        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(raw.get("system_message").is_none());
    }

    #[test]
    fn envelope_system_message_only() {
        let env = HookResponseEnvelope {
            result: None,
            system_message: Some("[warn] server offline".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: HookResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert!(parsed.result.is_none());
        assert_eq!(
            parsed.system_message.as_deref(),
            Some("[warn] server offline")
        );
        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(raw.get("result").is_none());
    }

    #[test]
    fn envelope_both_fields() {
        let env = HookResponseEnvelope {
            result: Some(HookResult::Cleared(2)),
            system_message: Some("─── background ───\n[warn] offline".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: HookResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.result, Some(HookResult::Cleared(2)));
        assert!(
            parsed
                .system_message
                .as_ref()
                .is_some_and(|m| m.contains("offline"))
        );
    }

    #[test]
    fn envelope_empty_is_default() {
        let env = HookResponseEnvelope::default();
        assert!(env.result.is_none());
        assert!(env.system_message.is_none());
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(json, "{}");
    }

    // ── Per-host response shape tests ──────────────────────────────────

    #[test]
    fn claude_code_response_shape() {
        // Stop hook allow with background drain.
        let env = HookResponseEnvelope {
            result: None,
            system_message: Some("─── background ───\n[warn] ra offline".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        // Claude Code reads systemMessage from the hook response.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(
            parsed["system_message"].as_str(),
            Some("─── background ───\n[warn] ra offline"),
        );
    }

    #[test]
    fn gemini_cli_response_shape() {
        // AfterAgent hook allow with background drain.
        let env = HookResponseEnvelope {
            result: None,
            system_message: Some("─── background ───\n[err] pylsp crashed".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(
            parsed["system_message"].as_str(),
            Some("─── background ───\n[err] pylsp crashed"),
        );
    }

    // ── Outcome-based level tests ──────────────────────────────────────

    #[test]
    fn hook_allow_emits_at_debug() {
        // Empty envelope = allow (no result, no system_message)
        let env = HookResponseEnvelope::default();
        let level = HookServer::hook_outcome_level("pre-tool/editing-state", &env);
        assert_eq!(level, tracing::Level::DEBUG);
    }

    #[test]
    fn hook_block_emits_at_info() {
        let env = HookResponseEnvelope {
            result: Some(HookResult::Deny("call editing start first".into())),
            system_message: None,
        };
        let level = HookServer::hook_outcome_level("pre-tool/editing-state", &env);
        assert_eq!(level, tracing::Level::INFO);
    }

    #[test]
    fn hook_turn_start_debug_without_result() {
        // turn-start with no result → debug (lifecycle category, empty result)
        let env = HookResponseEnvelope {
            result: None,
            system_message: None,
        };
        let level = HookServer::hook_outcome_level("pre-agent/turn-start", &env);
        assert_eq!(level, tracing::Level::DEBUG);
    }

    // ── Host payload capture tests ────────────────────────────────────

    #[test]
    fn hook_request_tolerates_host_payload() {
        // HookRequest deserialization silently ignores `host_payload` —
        // no deny_unknown_fields — so the field survives in the raw Value
        // for logging without breaking dispatch deserialization.
        let json = r#"{
            "method": "pre-agent/turn-start",
            "host_payload": {
                "transcript_path": "/tmp/transcript.jsonl",
                "cwd": "/home/user/project",
                "hook_event_name": "UserPromptSubmit"
            }
        }"#;
        let req: HookRequest = serde_json::from_str(json).expect("should deserialize");
        assert!(matches!(req, HookRequest::PreAgent { session_id: None }));

        // The raw Value retains host_payload for protocol logging.
        let raw: serde_json::Value = serde_json::from_str(json).expect("parse raw");
        assert_eq!(
            raw["host_payload"]["transcript_path"].as_str(),
            Some("/tmp/transcript.jsonl"),
        );
    }

    #[test]
    fn hook_event_stores_host_payload() {
        let (_logging, conn, _guard) = setup_logging();

        let payload = serde_json::json!({
            "method": "pre-agent/turn-start",
            "host_payload": {
                "transcript_path": "/tmp/transcript.jsonl",
                "cwd": "/home/user/project",
                "hook_event_name": "UserPromptSubmit"
            }
        });

        emit_hook_event(
            tracing::Level::DEBUG,
            "claude-code",
            "pre-agent/turn-start",
            None,
            &payload.to_string(),
            "incoming hook",
        );

        let rows = hook_messages(&conn);
        assert_eq!(rows.len(), 1);

        // Verify the stored payload contains the nested host data.
        let stored_payload: String = {
            let c = conn.lock().expect("lock");
            c.query_row("SELECT payload FROM messages WHERE id = 1", [], |row| {
                row.get(0)
            })
            .expect("query payload")
        };
        let stored: serde_json::Value =
            serde_json::from_str(&stored_payload).expect("parse stored payload");
        assert_eq!(
            stored["host_payload"]["transcript_path"].as_str(),
            Some("/tmp/transcript.jsonl"),
        );
        assert_eq!(
            stored["host_payload"]["hook_event_name"].as_str(),
            Some("UserPromptSubmit"),
        );
    }

    // ── IPC server tests ─────────────────────────────────────────────

    use crate::bridge::session::Session;
    use crate::config::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open test DB");
        (dir, path, conn)
    }

    /// Shared test context holding a `HookServer` and all lifetime-bound
    /// resources.
    struct TestHookServer {
        server: Option<HookServer>,
        _db_dir: tempfile::TempDir,
    }

    /// Create a `HookServer` backed by an in-memory session.
    ///
    /// The `Session` has no LSP servers but a functional notification
    /// router and config version counter — enough to exercise the IPC
    /// dispatch and root sync paths.
    fn test_hook_server() -> TestHookServer {
        let (db_dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let config = Config::default();
        let logging = crate::logging::LoggingServer::new();
        let handle = tokio::runtime::Handle::current();
        let instance_id: Arc<str> = "test-session".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        notification_router.register_session(&instance_id);

        let session = Arc::new(Session::new(
            config,
            vec![],
            logging,
            conn.clone(),
            instance_id.clone(),
            handle,
            notification_router,
        ));
        let server = HookServer::new(session, conn, instance_id, "test".to_string());

        TestHookServer {
            server: Some(server),
            _db_dir: db_dir,
        }
    }

    /// Send a JSON request through a duplex stream and run
    /// `handle_connection` on the server side. Returns the response.
    async fn ipc_exchange(server: &Arc<HookServer>, request: &str) -> String {
        let (mut client, server_side) = tokio::io::duplex(4096);

        let server_clone = server.clone();
        let req = format!("{request}\n");
        let handle = tokio::spawn(async move {
            server_clone
                .handle_connection(server_side)
                .await
                .expect("handle_connection");
        });

        client
            .write_all(req.as_bytes())
            .await
            .expect("write request");

        handle.await.expect("join");

        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("read response");
        response
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_accepts_and_processes_connection() {
        let mut ctx = test_hook_server();
        let server = ctx.server.take().expect("server");

        let sock_dir = tempfile::tempdir().expect("sock_dir");
        let socket_path = sock_dir.path().join("hook.sock");

        let _handle = server.start(&socket_path).expect("start");

        // Give the listener a moment to bind.
        tokio::task::yield_now().await;

        let mut stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect to hook socket");

        // Send a session-start request (handler returns empty envelope
        // when there is no editing state and no pending notifications).
        let request = r#"{"method": "session-start/clear-editing"}"#;
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("write request");
        stream.shutdown().await.expect("shutdown write");

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read response");

        // handle_connection always writes a trailing newline, even for
        // empty (allow) responses.
        assert!(
            response.ends_with('\n'),
            "expected response ending with newline, got: {response:?}",
        );
    }

    #[tokio::test]
    async fn handle_connection_processes_request() {
        let ctx = test_hook_server();
        let server = Arc::new(ctx.server.expect("server"));

        let response = ipc_exchange(&server, r#"{"method": "session-start/clear-editing"}"#).await;

        // handle_connection always writes a trailing newline, even for
        // empty (allow) responses.
        assert!(
            response.ends_with('\n'),
            "expected response ending with newline, got: {response:?}",
        );
    }
}
