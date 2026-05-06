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
use crate::source::Source;

/// Emit a hook protocol event at the given tracing level.
///
/// Protocol routing is by `kind` field — `MessageDbSink` matches
/// `kind in {lsp, mcp, hook}` regardless of tracing level.
/// The level controls DB `level` column and TUI filtering threshold.
pub(crate) fn emit_hook_event(
    level: tracing::Level,
    client_name: &str,
    method: &str,
    request_id: i64,
    parent_id: Option<i64>,
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
            request_id = request_id,
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
            request_id = request_id,
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
            request_id = request_id,
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
            request_id = request_id,
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
        /// Path to the host CLI's transcript file (Claude Code only).
        /// Used for transcript-based `/add-dir` root detection.
        #[serde(default)]
        transcript_path: Option<String>,
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
        /// Host CLI working directory. Stashed for Catenary grep/glob
        /// calls so the MCP handler can resolve relative patterns.
        #[serde(default)]
        cwd: Option<String>,
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

    /// LSP diagnostics for a changed file.
    #[serde(rename = "post-tool/diagnostics")]
    PostTool {
        /// Absolute path to the changed file.
        file: String,
        /// Name of the host CLI tool that triggered the hook.
        /// Used for file accumulation during editing mode and logged
        /// in the payload for monitor visibility.
        #[serde(default)]
        tool: Option<String>,
        /// Agent ID (empty string for the main agent).
        #[serde(default)]
        agent_id: String,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Force `done_editing` before the agent stops.
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

    /// Clear stale editing state on session start.
    #[serde(rename = "session-start/clear-editing")]
    SessionStart {
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

        // Mint a correlation ID for this request/response pair
        let id = self.router.session.logging.next_id();

        let request: HookRequest = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("Invalid hook request: {e}"))?;

        let result = self.router.dispatch(request, id.0);

        // Apply transcript-discovered roots before responding to the hook.
        // Runs async in the hook server task — sync_roots is the single
        // serialization point for root updates.
        if !result.add_roots.is_empty() {
            let session = &self.router.session;
            let mut current = session.roots();
            let before = current.len();
            for root in &result.add_roots {
                if !current.contains(root) {
                    current.push(root.clone());
                }
            }
            let added = current.len() - before;
            if added > 0 {
                debug!(
                    source = Source::HookDispatch.as_str(),
                    added, "transcript root sync: syncing new roots",
                );
                if let Err(e) = session.sync_roots(current).await {
                    debug!(
                        source = Source::HookDispatch.as_str(),
                        "transcript root sync failed: {e}",
                    );
                }
            }
        }

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
            id.0,
            None,
            &raw.to_string(),
            "incoming hook",
        );

        // Log outgoing hook response
        emit_hook_event(
            level,
            &self.router.client_name,
            &method,
            id.0,
            Some(id.0),
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
        // diagnostics / lifecycle: non-empty result → info, empty → debug
        "diagnostics" | "lifecycle" => {
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
        let original = HookResult::Deny("call start_editing first".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn hook_result_block_round_trip() {
        let original = HookResult::Block("call done_editing first".into());
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
        // pre-agent/turn-start (no transcript_path, no session_id)
        let json = r#"{"method": "pre-agent/turn-start"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("turn-start");
        assert!(matches!(
            req,
            HookRequest::PreAgent {
                transcript_path: None,
                session_id: None,
            }
        ));

        // pre-agent/turn-start with transcript_path
        let json =
            r#"{"method": "pre-agent/turn-start", "transcript_path": "/tmp/transcript.jsonl"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("turn-start with transcript");
        assert!(matches!(
            req,
            HookRequest::PreAgent {
                transcript_path: Some(ref p),
                session_id: None,
            } if p == "/tmp/transcript.jsonl"
        ));

        // pre-agent/turn-start with session_id
        let json = r#"{"method": "pre-agent/turn-start", "session_id": "sess-123"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("turn-start with session_id");
        assert!(matches!(
            req,
            HookRequest::PreAgent {
                transcript_path: None,
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
            cwd,
        } = req
        else {
            unreachable!("expected PreTool");
        };
        assert_eq!(tool_name, "Edit");
        assert_eq!(file_path.as_deref(), Some("/tmp/foo.rs"));
        assert!(command.is_none());
        assert_eq!(agent_id, "");
        assert_eq!(session_id.as_deref(), Some("abc123"));
        assert!(cwd.is_none());

        // pre-tool/editing-state with command (Bash tool)
        let json = r#"{"method": "pre-tool/editing-state", "tool_name": "Bash", "command": "rm -rf target/", "agent_id": ""}"#;
        let req: HookRequest = serde_json::from_str(json).expect("editing-state with command");
        let HookRequest::PreTool { command, .. } = req else {
            unreachable!("expected PreTool");
        };
        assert_eq!(command.as_deref(), Some("rm -rf target/"));

        // pre-tool/editing-state with cwd (grep/glob cwd resolution)
        let json = r#"{"method": "pre-tool/editing-state", "tool_name": "mcp__plugin_catenary_catenary__grep", "agent_id": "", "cwd": "/home/user/project"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("editing-state with cwd");
        let HookRequest::PreTool { cwd, .. } = req else {
            unreachable!("expected PreTool");
        };
        assert_eq!(cwd.as_deref(), Some("/home/user/project"));

        // post-tool/diagnostics with optional fields
        let json =
            r#"{"method": "post-tool/diagnostics", "file": "/tmp/test.rs", "tool": "Write"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("diagnostics");
        let HookRequest::PostTool { file, tool, .. } = req else {
            unreachable!("expected PostTool");
        };
        assert_eq!(file, "/tmp/test.rs");
        assert_eq!(tool.as_deref(), Some("Write"));

        // post-tool/diagnostics without optional fields
        let json = r#"{"method": "post-tool/diagnostics", "file": "/tmp/test.rs"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("diagnostics minimal");
        let HookRequest::PostTool { tool, .. } = req else {
            unreachable!("expected PostTool");
        };
        assert!(tool.is_none());

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
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();
        emit_hook_event(
            tracing::Level::INFO,
            "claude-code",
            "post-tool/diagnostics",
            id.0,
            None,
            &serde_json::json!({
                "method": "post-tool/diagnostics",
                "file": "/tmp/test.rs",
                "tool": "Write"
            })
            .to_string(),
            "incoming hook",
        );

        let rows = hook_messages(&conn);
        assert!(!rows.is_empty(), "should have at least the hook row");
        assert_eq!(rows[0].method, "post-tool/diagnostics");
        assert_eq!(rows[0].client, "claude-code");
    }

    #[test]
    fn hook_pair_merges() {
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();

        // Incoming request
        emit_hook_event(
            tracing::Level::INFO,
            "claude-code",
            "post-tool/diagnostics",
            id.0,
            None,
            &serde_json::json!({
                "method": "post-tool/diagnostics",
                "file": "/tmp/test.rs"
            })
            .to_string(),
            "incoming hook",
        );

        // Outgoing response
        emit_hook_event(
            tracing::Level::INFO,
            "claude-code",
            "post-tool/diagnostics",
            id.0,
            Some(id.0),
            &serde_json::json!({"content": "[clean]"}).to_string(),
            "outgoing hook response",
        );

        let rows = hook_messages(&conn);
        assert!(
            rows.len() >= 2,
            "should have at least request + response, got {}",
            rows.len()
        );
        // Both share the same request_id
        assert_eq!(rows[0].request_id, Some(id.0));
        assert_eq!(rows[1].request_id, Some(id.0));
        // Response has parent_id pointing back
        assert!(rows[0].parent_id.is_none());
        assert_eq!(rows[1].parent_id, Some(id.0));
    }

    #[test]
    fn hook_turn_start_writes_protocol_row() {
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();
        emit_hook_event(
            tracing::Level::INFO,
            "host",
            "pre-agent/turn-start",
            id.0,
            None,
            &serde_json::json!({"method": "pre-agent/turn-start"}).to_string(),
            "incoming hook",
        );

        emit_hook_event(
            tracing::Level::INFO,
            "host",
            "pre-agent/turn-start",
            id.0,
            Some(id.0),
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
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();
        emit_hook_event(
            tracing::Level::DEBUG,
            "test",
            "post-tool/diagnostics",
            id.0,
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
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();
        emit_hook_event(
            tracing::Level::INFO,
            "test",
            "post-tool/diagnostics",
            id.0,
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
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();
        emit_hook_event(
            tracing::Level::WARN,
            "test",
            "post-tool/diagnostics",
            id.0,
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
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();
        emit_hook_event(
            tracing::Level::ERROR,
            "test",
            "post-tool/diagnostics",
            id.0,
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
            result: Some(HookResult::Deny("call start_editing first".into())),
            system_message: None,
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: HookResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            parsed.result,
            Some(HookResult::Deny("call start_editing first".into()))
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
            result: Some(HookResult::Deny("call start_editing first".into())),
            system_message: None,
        };
        let level = HookServer::hook_outcome_level("pre-tool/editing-state", &env);
        assert_eq!(level, tracing::Level::INFO);
    }

    #[test]
    fn hook_diagnostics_result_emits_at_info() {
        let env = HookResponseEnvelope {
            result: Some(HookResult::Cleared(1)),
            system_message: None,
        };
        let level = HookServer::hook_outcome_level("post-tool/diagnostics", &env);
        assert_eq!(level, tracing::Level::INFO);
    }

    #[test]
    fn hook_diagnostics_clean_emits_at_debug() {
        // Clean diagnostics return no result (empty response)
        let env = HookResponseEnvelope::default();
        let level = HookServer::hook_outcome_level("post-tool/diagnostics", &env);
        assert_eq!(level, tracing::Level::DEBUG);
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
        assert!(matches!(
            req,
            HookRequest::PreAgent {
                transcript_path: None,
                session_id: None,
            }
        ));

        // The raw Value retains host_payload for protocol logging.
        let raw: serde_json::Value = serde_json::from_str(json).expect("parse raw");
        assert_eq!(
            raw["host_payload"]["transcript_path"].as_str(),
            Some("/tmp/transcript.jsonl"),
        );
    }

    #[test]
    fn hook_event_stores_host_payload() {
        let (logging, conn, _guard) = setup_logging();

        let id = logging.next_id();
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
            id.0,
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
}
