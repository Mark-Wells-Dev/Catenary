// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Transport layer: process lifecycle, reader loop, request/response correlation.

use anyhow::{Context, Result, anyhow};
use bytes::BytesMut;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, info, warn};

use crate::protocol::category::{lsp_category, lsp_category_level, window_message_level};

use tokio_util::sync::CancellationToken;

use super::protocol::{self, RequestId, RequestMessage, ResponseError, ResponseMessage};
use super::server::LspServer;
use crate::logging::LoggingServer;
use crate::mcp::RequestCancelled;

/// Tracks an in-flight request so we can annotate the response with
/// the original method name and scope identity.
struct PendingRequest {
    method: String,
    /// UUID shared by both request and response events for pair-merge.
    parent_id: Option<String>,
    sender: oneshot::Sender<ResponseMessage>,
}

/// Emit an LSP protocol event at the given tracing level.
///
/// Protocol routing is by `kind` field, not by level — the JSONL firehose
/// sink keys `kind in {lsp, mcp, hook}` regardless of tracing level. The
/// level sets the record's `level` and the TUI filtering threshold.
///
/// `source` is an optional subsystem tag (e.g., `lsp.logging`). Most
/// callers pass `None`; `window/logMessage` passes its source tag.
#[allow(clippy::too_many_arguments, reason = "protocol event boundary")]
fn emit_lsp_event(
    level: tracing::Level,
    server_name: &str,
    method: &str,
    parent_id: Option<&str>,
    scope_root: Option<&str>,
    source: Option<&str>,
    payload: &str,
    msg: &str,
) {
    if level == tracing::Level::ERROR {
        crate::emit_protocol_event!(
            error,
            kind = "lsp",
            method = method,
            server = server_name,
            client = "catenary",
            parent_id = parent_id,
            scope_root = scope_root,
            source = source,
            payload = payload,
            "{msg}"
        );
    } else if level == tracing::Level::WARN {
        crate::emit_protocol_event!(
            warn,
            kind = "lsp",
            method = method,
            server = server_name,
            client = "catenary",
            parent_id = parent_id,
            scope_root = scope_root,
            source = source,
            payload = payload,
            "{msg}"
        );
    } else if level == tracing::Level::INFO {
        crate::emit_protocol_event!(
            info,
            kind = "lsp",
            method = method,
            server = server_name,
            client = "catenary",
            parent_id = parent_id,
            scope_root = scope_root,
            source = source,
            payload = payload,
            "{msg}"
        );
    } else {
        crate::emit_protocol_event!(
            debug,
            kind = "lsp",
            method = method,
            server = server_name,
            client = "catenary",
            parent_id = parent_id,
            scope_root = scope_root,
            source = source,
            payload = payload,
            "{msg}"
        );
    }
}

/// Whether an incoming notification should be omitted from the firehose.
///
/// `$/progress` `report` ticks are the chattiest LSP source; their live
/// percentage lives in `state.json` (observability ticket 04) instead, so they
/// are not appended to the JSONL/DB firehose. `begin`/`end` are kept for
/// forensics, and every other notification emits normally. The notification is
/// still dispatched to [`LspServer::on_notification`] regardless — only the
/// firehose line is suppressed.
fn suppresses_firehose(method: &str, value: &serde_json::Value) -> bool {
    method == "$/progress"
        && value
            .get("params")
            .and_then(|p| p.get("value"))
            .and_then(|v| v.get("kind"))
            .and_then(serde_json::Value::as_str)
            == Some("report")
}

/// Extract the scope root path string from an `LspServer`.
///
/// Returns `Some(path)` for `Scope::Root` instances, `None` otherwise.
fn scope_root_str(server: &LspServer) -> Option<String> {
    server
        .scope()
        .and_then(|s| s.root_path())
        .map(|p| p.display().to_string())
}

/// Whether an LSP error code indicates a retriable condition.
///
/// - `-32801` (`ContentModified`): file changed during request.
/// - `-32800` (`RequestCancelled`): server cancelled the request.
///
/// Both are transient — the request may succeed on retry after the
/// server settles.
const fn is_retriable_lsp_error(code: i64) -> bool {
    code == -32801 || code == -32800
}

/// A JSON-RPC error *response* from the server — the server received the
/// request, processed it, and answered with an error object.
///
/// This is categorically distinct from a transport failure (server death,
/// stuck server, closed connection): an error *response* is proof the server
/// is alive and answering, which is exactly what a liveness probe verifies.
/// [`Connection::request`] returns this (via `anyhow`) for every non-retriable
/// error response, so a caller can `downcast_ref` to tell "the server said no"
/// (e.g. `-32601 Method not found` for an unadvertised capability) from "the
/// server is gone". The [`std::fmt::Display`] output is byte-identical to the
/// prior inline `anyhow!` message, so logs and existing assertions are
/// unchanged.
#[derive(Debug, thiserror::Error)]
#[error("[{language}] LSP error {code}: {message}")]
pub struct LspResponseError {
    /// The server's JSON-RPC error code (e.g. `-32601` for method-not-found).
    pub code: i64,
    /// The language tag the connection carries (for the message prefix).
    pub language: String,
    /// The server's human-readable error message.
    pub message: String,
}

/// Poll interval for failure detection sampling.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// CPU tick threshold for request timeout: 1000 ticks = 10 CPU-seconds.
const REQUEST_THRESHOLD: u64 = 1000;

/// Owns the LSP server process, the reader loop, and request/response
/// correlation. Knows about JSON-RPC framing but nothing about LSP
/// semantics.
pub struct Connection {
    pid: Option<u32>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    alive: Arc<AtomicBool>,
    next_id: AtomicI64,
    server: Weak<LspServer>,
    language: String,
    _logging: LoggingServer,
    server_name: String,
    monitor: std::sync::Mutex<Option<catenary_proc::ProcessMonitor>>,
    /// Write end of the stdout pipe, for injecting drain sentinels.
    ///
    /// Created at spawn time by `os_pipe::pipe()` and `try_clone()`.
    /// One copy goes to the child as its stdout; this copy stays with
    /// [`Connection`] so [`Self::drain`] can write a sentinel response
    /// that the reader loop picks up in FIFO order.
    ///
    /// Wrapped in `Arc` so the child-exit task can close it when the
    /// server dies — necessary because the extra write fd would
    /// otherwise prevent EOF detection on the reader loop.
    drain_writer: Arc<std::sync::Mutex<Option<os_pipe::PipeWriter>>>,
    /// Signals the child-exit task to kill the server process.
    /// Fired in [`Drop`] so the task can call [`Child::start_kill`].
    kill_token: CancellationToken,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl Connection {
    /// Spawn a server process and start the reader loop.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server process cannot be spawned.
    /// - Stdin or stdout cannot be captured.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "spawn parameters from ServerDef"
    )]
    pub fn new(
        program: &str,
        args: &[&str],
        stderr: Stdio,
        env: Option<&HashMap<String, String>>,
        server: &Arc<LspServer>,
        language: String,
        logging: LoggingServer,
        server_name: &str,
        scope_root: &str,
    ) -> Result<(Self, Option<ChildStderr>)> {
        // Create the stdout pipe ourselves so we can keep the write end
        // for drain sentinel injection. The child gets one copy of the
        // write end (as its stdout); we keep the other for `drain()`.
        let (pipe_reader, pipe_writer) = os_pipe::pipe().context("Failed to create stdout pipe")?;
        let drain_writer = pipe_writer
            .try_clone()
            .context("Failed to clone stdout pipe writer")?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(pipe_writer)
            .stderr(stderr);
        if let Some(env) = env {
            cmd.envs(env);
        }
        catenary_proc::set_parent_death_signal(cmd.as_std_mut());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn LSP server: {program}"))?;

        let pid = child.id();
        if let Some(pid) = pid {
            catenary_proc::register_child_process(pid);
        }

        let monitor = pid.and_then(catenary_proc::ProcessMonitor::new);

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("stdin not captured"))?;

        // Convert the pipe read end to a tokio async reader.
        // Unix: epoll-backed Receiver (zero-copy wakeup).
        // Windows: threadpool-backed File (anonymous pipes don't
        //   support IOCP, same as ChildStdout internally).
        let stdout =
            to_async_reader(pipe_reader).context("Failed to register stdout pipe with tokio")?;

        let child_stderr = child.stderr.take();

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        let weak_server = Arc::downgrade(server);

        let reader_handle = tokio::spawn(Self::reader_loop(
            stdin.clone(),
            pending.clone(),
            alive.clone(),
            Arc::downgrade(server),
            stdout,
            logging.clone(),
            server_name.to_string(),
            scope_root.to_string(),
        ));

        let drain_writer = Arc::new(std::sync::Mutex::new(Some(drain_writer)));

        // Background task: owns the Child handle. Waits for either
        // natural exit or a kill signal (from Drop), then closes the
        // drain write end so the reader loop sees EOF.
        let kill_token = CancellationToken::new();
        {
            let drain_for_close = drain_writer.clone();
            let kill = kill_token.clone();
            let exit_server_name = server_name.to_string();
            let exit_scope_root = scope_root.to_string();
            tokio::spawn(async move {
                let sr = if exit_scope_root.is_empty() {
                    None
                } else {
                    Some(exit_scope_root.as_str())
                };
                tokio::select! {
                    status = child.wait() => {
                        match status {
                            Ok(s) if !s.success() => {
                                info!(
                                    source = "lsp.lifecycle",
                                    server = exit_server_name.as_str(),
                                    scope_root = sr,
                                    "{exit_server_name}: exited with {s}",
                                );
                            }
                            Err(e) => {
                                info!(
                                    source = "lsp.lifecycle",
                                    server = exit_server_name.as_str(),
                                    scope_root = sr,
                                    "{exit_server_name}: wait failed: {e}",
                                );
                            }
                            _ => {}
                        }
                    }
                    () = kill.cancelled() => {
                        let _ = child.start_kill();
                    }
                }
                *drain_for_close
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            });
        }

        Ok((
            Self {
                pid,
                stdin,
                pending,
                alive,
                next_id: AtomicI64::new(1),
                server: weak_server,
                language,
                _logging: logging,
                server_name: server_name.to_string(),
                monitor: std::sync::Mutex::new(monitor),
                drain_writer,
                kill_token,
                _reader_handle: reader_handle,
            },
            child_stderr,
        ))
    }

    /// Send a request and wait for the response with failure detection.
    ///
    /// Uses CPU-tick failure detection via [`ProcessMonitor`](catenary_proc::ProcessMonitor).
    /// When the monitor is unavailable, falls back to reader-loop death
    /// detection (`is_alive`). Retries on `ContentModified` (-32801) or
    /// `RequestCancelled` (-32800).
    ///
    /// If `cancel` is triggered (MCP client cancelled the tool call),
    /// sends `$/cancelRequest` to the LSP server and returns
    /// [`RequestCancelled`].
    #[allow(
        clippy::too_many_lines,
        reason = "Request retry logic with failure detection"
    )]
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        parent_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value> {
        let server = self
            .server
            .upgrade()
            .ok_or_else(|| anyhow!("[{}] server dropped", self.language))?;

        let scope_root_str = scope_root_str(&server);
        let scope_root_ref = scope_root_str.as_deref();

        // Retry loop for ContentModified errors
        for _attempt in 0..3 {
            let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));

            let request = RequestMessage {
                jsonrpc: "2.0".to_string(),
                id: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            };

            let level = lsp_category_level(lsp_category(method));
            if let Ok(payload) = serde_json::to_value(&request) {
                emit_lsp_event(
                    level,
                    &self.server_name,
                    method,
                    parent_id,
                    scope_root_ref,
                    None,
                    &payload.to_string(),
                    "outgoing request",
                );
            }

            let (tx, rx) = oneshot::channel();
            {
                let mut pending = self.pending.lock().await;
                pending.insert(
                    id.clone(),
                    PendingRequest {
                        method: method.to_string(),
                        parent_id: parent_id.map(str::to_string),
                        sender: tx,
                    },
                );
            }

            self.send_message(&request).await?;

            // Wait for response: select on rx + failure detection timer
            let response = {
                let mut rx = rx;
                let mut budget = i64::try_from(REQUEST_THRESHOLD).unwrap_or(1000);

                loop {
                    tokio::select! {
                        result = &mut rx => {
                            match result {
                                Ok(resp) => break Ok(resp),
                                Err(_) => break Err(anyhow!(
                                    "[{}] server closed connection", self.language
                                )),
                            }
                        }
                        () = cancel.cancelled() => {
                            // MCP client cancelled the tool call.
                            // Send $/cancelRequest and clean up.
                            self.pending.lock().await.remove(&id);
                            let notif = Self::cancel_notification(&id);
                            let _ = self.send_message(&notif).await;
                            debug!("[{}] sent $/cancelRequest for {:?}", self.language, id);
                            break Err(RequestCancelled.into());
                        }
                        () = tokio::time::sleep(POLL_INTERVAL) => {
                            // Failure detection
                            if let Some(d) = self.sample_monitor() {
                                if d.state == catenary_proc::ProcessState::Dead {
                                    self.pending.lock().await.remove(&id);
                                    break Err(anyhow!(
                                        "[{}] server died during '{method}'",
                                        self.language
                                    ));
                                }
                                let delta = d.delta_utime + d.delta_stime;
                                if d.state == catenary_proc::ProcessState::Running
                                    && delta > 0
                                    && !server.is_progress_active()
                                {
                                    budget -= i64::try_from(delta)
                                        .unwrap_or(budget);
                                }
                            } else if !self.is_alive() {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] server died during '{method}'",
                                    self.language
                                ));
                            }

                            if budget <= 0 {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] request '{method}' failed \
                                     (server stuck)",
                                    self.language
                                ));
                            }
                        }
                    }
                }
            }?;

            if let Some(error) = response.error {
                if is_retriable_lsp_error(error.code) {
                    debug!("LSP request '{}' cancelled/modified, retrying...", method,);
                    tokio::select! {
                        () = server.state_notify().notified() => {}
                        () = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                    continue;
                }
                // An error *response* — the server answered. Carry it as a
                // typed error so a liveness probe can tell "the server said no"
                // (alive) from a transport failure (gone). Display is unchanged.
                return Err(LspResponseError {
                    code: error.code,
                    language: self.language.clone(),
                    message: error.message,
                }
                .into());
            }

            return Ok(response.result.unwrap_or(serde_json::Value::Null));
        }

        Err(anyhow!(
            "[{}] request '{method}' failed after retries",
            self.language
        ))
    }

    /// Send a notification (no response expected).
    pub async fn notify(
        &self,
        method: &str,
        params: serde_json::Value,
        parent_id: Option<&str>,
    ) -> Result<()> {
        let notification = super::protocol::NotificationMessage {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };
        if let Ok(payload) = serde_json::to_value(&notification) {
            let sr = self.server.upgrade().as_deref().and_then(scope_root_str);
            emit_lsp_event(
                tracing::Level::DEBUG,
                &self.server_name,
                method,
                parent_id,
                sr.as_deref(),
                None,
                &payload.to_string(),
                "outgoing notification",
            );
        }
        self.send_message(&notification).await
    }

    /// Whether the server process is alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Returns a shared reference to the alive flag.
    pub fn alive_flag(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }

    /// Sample the process monitor for CPU-tick failure detection.
    ///
    /// Returns [`ProcessDelta`](catenary_proc::ProcessDelta) with per-counter
    /// deltas since the last sample. Returns `None` if the process is gone
    /// or monitoring is unavailable.
    pub fn sample_monitor(&self) -> Option<catenary_proc::ProcessDelta> {
        self.monitor.lock().ok()?.as_mut()?.sample()
    }

    /// PID of the server process, captured at spawn time.
    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Ensures all bytes currently in the stdout pipe have been read
    /// and dispatched by the reader loop.
    ///
    /// Writes a synthetic JSON-RPC response into the pipe's write end
    /// (kept from spawn time) with a unique request ID. A matching
    /// oneshot is registered in `pending` beforehand. Because the pipe
    /// is FIFO, the reader loop must process every preceding byte —
    /// including any final `publishDiagnostics` notifications from the
    /// server — before it reaches the sentinel.
    ///
    /// Returns `Ok(())` when the sentinel has been consumed. Fails if
    /// the pipe write end is unavailable (server already dropped) or the
    /// reader loop has exited.
    pub async fn drain(&self) -> Result<()> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));

        // Register the oneshot BEFORE writing the sentinel so the
        // reader loop finds the entry when it processes the response.
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                id.clone(),
                PendingRequest {
                    method: "drain".to_string(),
                    parent_id: None,
                    sender: tx,
                },
            );
        }

        let response = super::protocol::ResponseMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(id.clone()),
            result: Some(serde_json::Value::Null),
            error: None,
        };
        let body = serde_json::to_string(&response)?;
        let msg = format!("Content-Length: {}\r\n\r\n{body}", body.len());

        // Write the sentinel. The payload is <200 bytes into a 64 KB
        // kernel pipe buffer — the write syscall cannot block. The
        // std::sync::Mutex guard is dropped before the await below.
        let write_ok = {
            use std::io::Write;
            let mut guard = self
                .drain_writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.as_mut().is_some_and(|writer| {
                writer.write_all(msg.as_bytes()).is_ok() && writer.flush().is_ok()
            })
        };

        if !write_ok {
            self.pending.lock().await.remove(&id);
            return Err(anyhow!("drain: stdout pipe write end unavailable"));
        }

        // Wait for the reader loop to process the sentinel.
        rx.await
            .map_err(|_| anyhow!("drain: reader loop closed before processing sentinel"))?;
        Ok(())
    }

    /// Build a `$/cancelRequest` notification for the given request ID.
    fn cancel_notification(id: &RequestId) -> super::protocol::NotificationMessage {
        super::protocol::NotificationMessage {
            jsonrpc: "2.0".to_string(),
            method: "$/cancelRequest".to_string(),
            params: serde_json::json!({"id": id}),
        }
    }

    /// Send a JSON-RPC message with Content-Length header.
    async fn send_message<T: serde::Serialize + Sync>(&self, message: &T) -> Result<()> {
        let body = serde_json::to_string(message)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.flush().await?;
        drop(stdin);

        Ok(())
    }

    /// Background task that reads LSP messages and routes them.
    #[allow(
        clippy::too_many_lines,
        clippy::too_many_arguments,
        reason = "Internal task requires sequential message parsing and dispatch"
    )]
    async fn reader_loop<R: AsyncRead + Unpin>(
        stdin: Arc<Mutex<ChildStdin>>,
        pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
        alive: Arc<AtomicBool>,
        server: Weak<LspServer>,
        stdout: R,
        _logging: LoggingServer,
        server_name: String,
        scope_root: String,
    ) {
        let scope_root_opt = if scope_root.is_empty() {
            None
        } else {
            Some(scope_root)
        };
        let scope_root_ref = scope_root_opt.as_deref();
        let mut reader = BufReader::new(stdout);
        let mut buffer = BytesMut::with_capacity(8192);

        loop {
            // Read more data into buffer
            let mut temp = [0u8; 4096];
            match reader.read(&mut temp).await {
                Ok(0) => {
                    debug!("LSP stdout closed");
                    break;
                }
                Ok(n) => {
                    buffer.extend_from_slice(&temp[..n]);
                }
                Err(e) => {
                    info!(
                        source = "lsp.lifecycle",
                        server = server_name.as_str(),
                        scope_root = scope_root_ref,
                        "Error reading from LSP stdout: {e}",
                    );
                    break;
                }
            }

            // Try to parse complete messages
            loop {
                match protocol::try_parse_message(&mut buffer) {
                    Ok(None) => break, // Need more data
                    Err(e) => {
                        let dump_len = buffer.len().min(128);
                        warn!(
                            server = server_name.as_str(),
                            source = "lsp.protocol",
                            scope_root = scope_root_ref,
                            "malformed LSP message from {server_name}, \
                             resynchronizing: {e}"
                        );
                        debug!(
                            server = server_name.as_str(),
                            buffer_len = buffer.len(),
                            "buffer head (hex): {:02x?}",
                            &buffer[..dump_len]
                        );
                        protocol::resync_to_next_message(&mut buffer);
                    }
                    Ok(Some(message_str)) => {
                        let value: serde_json::Value = match serde_json::from_str(&message_str) {
                            Ok(v) => v,
                            Err(e) => {
                                // A framing-valid message whose body is not JSON
                                // is DROPPED — and a dropped frame can strand a
                                // pipeline waiting on a response (bug 95). Make
                                // it a health finding, not a silent debug line.
                                let preview: String = message_str.chars().take(96).collect();
                                warn!(
                                    server = server_name.as_str(),
                                    source = crate::source::Source::LspDispatch.as_str(),
                                    scope_root = scope_root_ref,
                                    byte_len = message_str.len(),
                                    body_preview = preview.as_str(),
                                    "dropping framing-valid but non-JSON LSP \
                                     message from {server_name}: {e}"
                                );
                                continue;
                            }
                        };

                        // Upgrade weak reference — if LspServer is gone, exit
                        let Some(server) = server.upgrade() else {
                            debug!("LspServer dropped, reader loop exiting");
                            break;
                        };

                        let sr = scope_root_str(&server);
                        let sr_ref = sr.as_deref();

                        // Check message type
                        if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
                            // Request or Notification
                            if let Some(id) = value.get("id") {
                                // Server Request — always debug (server-initiated plumbing)
                                debug!("Received server request: {} (id: {})", method, id);
                                let exchange_id = uuid::Uuid::new_v4().to_string();
                                emit_lsp_event(
                                    tracing::Level::DEBUG,
                                    &server_name,
                                    method,
                                    Some(&exchange_id),
                                    sr_ref,
                                    None,
                                    &value.to_string(),
                                    "incoming server request",
                                );

                                let request_id = serde_json::from_value(id.clone())
                                    .unwrap_or(RequestId::Number(0));

                                let params =
                                    value.get("params").unwrap_or(&serde_json::Value::Null);

                                let response = match server.on_request(method, params) {
                                    Ok(result) => ResponseMessage {
                                        jsonrpc: "2.0".to_string(),
                                        id: Some(request_id),
                                        result: Some(result),
                                        error: None,
                                    },
                                    Err(e) => ResponseMessage {
                                        jsonrpc: "2.0".to_string(),
                                        id: Some(request_id),
                                        result: None,
                                        error: Some(ResponseError {
                                            code: e.code,
                                            message: e.message,
                                            data: None,
                                        }),
                                    },
                                };

                                // Log outbound response (same level as request)
                                if let Ok(response_json) = serde_json::to_value(&response) {
                                    emit_lsp_event(
                                        tracing::Level::DEBUG,
                                        &server_name,
                                        method,
                                        Some(&exchange_id),
                                        sr_ref,
                                        None,
                                        &response_json.to_string(),
                                        "outgoing server response",
                                    );
                                }

                                if let Ok(body) = serde_json::to_string(&response) {
                                    let header = format!("Content-Length: {}\r\n\r\n", body.len());
                                    let mut stdin_guard = stdin.lock().await;
                                    if let Err(e) = stdin_guard.write_all(header.as_bytes()).await {
                                        debug!("Failed to write response header: {}", e);
                                    } else if let Err(e) =
                                        stdin_guard.write_all(body.as_bytes()).await
                                    {
                                        debug!("Failed to write response body: {}", e);
                                    } else if let Err(e) = stdin_guard.flush().await {
                                        debug!("Failed to flush response: {}", e);
                                    }
                                }
                            } else {
                                // Notification — level and source determined by method.
                                let (notif_level, source) = match method {
                                    // Server-forwarded window messages carry the
                                    // `lsp.logging` source so the notification sinks
                                    // exclude them by origin (misc 125): server chatter
                                    // is firehose-only, never promoted to the user
                                    // queue, regardless of mapped severity. logMessage
                                    // stays at info; showMessage keeps its type-mapped
                                    // level so the firehose/TUI record its true severity.
                                    "window/logMessage" => (
                                        tracing::Level::INFO,
                                        Some(crate::source::Source::LspLogging.as_str()),
                                    ),
                                    "window/showMessage" => {
                                        let msg_type = value
                                            .get("params")
                                            .and_then(|p| p.get("type"))
                                            .and_then(serde_json::Value::as_u64);
                                        (
                                            window_message_level(msg_type),
                                            Some(crate::source::Source::LspLogging.as_str()),
                                        )
                                    }
                                    _ => (lsp_category_level(lsp_category(method)), None),
                                };
                                let msg = match method {
                                    "window/logMessage" | "window/showMessage" => {
                                        let text = value
                                            .get("params")
                                            .and_then(|p| p.get("message"))
                                            .and_then(serde_json::Value::as_str)
                                            .unwrap_or("(no message)");
                                        format!("{server_name}: {text}")
                                    }
                                    _ => format!("{server_name}: {method}"),
                                };
                                if !suppresses_firehose(method, &value) {
                                    emit_lsp_event(
                                        notif_level,
                                        &server_name,
                                        method,
                                        None,
                                        sr_ref,
                                        source,
                                        &value.to_string(),
                                        &msg,
                                    );
                                }
                                let params =
                                    value.get("params").unwrap_or(&serde_json::Value::Null);
                                server.on_notification(method, params);
                            }
                        } else if value.get("id").is_some() {
                            // Response — match the level of the outgoing request
                            if let Ok(response) =
                                serde_json::from_value::<ResponseMessage>(value.clone())
                                && let Some(id) = &response.id
                            {
                                let mut pending = pending.lock().await;
                                if let Some(req) = pending.remove(id) {
                                    // Drain sentinels are internal markers, not
                                    // LSP traffic — skip protocol logging.
                                    if req.method != "drain" {
                                        let resp_level =
                                            lsp_category_level(lsp_category(&req.method));
                                        emit_lsp_event(
                                            resp_level,
                                            &server_name,
                                            &req.method,
                                            req.parent_id.as_deref(),
                                            sr_ref,
                                            None,
                                            &value.to_string(),
                                            "incoming response",
                                        );
                                    }
                                    let _ = req.sender.send(response);
                                } else {
                                    debug!("Received response for unknown request id: {:?}", id);
                                }
                            }
                        } else {
                            debug!("Unknown message format: {}", message_str);
                        }
                    } // Ok(Some)
                } // match
            } // loop
        }

        // Mark server as dead and trigger shutdown cleanup
        alive.store(false, Ordering::SeqCst);
        if let Some(server) = server.upgrade() {
            server.on_shutdown();
        }
        info!(
            source = "lsp.lifecycle",
            server = server_name.as_str(),
            scope_root = scope_root_ref,
            "LSP reader task exiting - server connection lost",
        );
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Close the drain write end so the reader loop sees EOF.
        *self
            .drain_writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        // Signal the child-exit task to kill the server process.
        // The task calls `start_kill()` on the Child it owns.
        self.kill_token.cancel();
        // Synchronous fallback: if the runtime is shutting down and
        // the task can't run, kill by PID directly.
        if let Some(pid) = self.pid {
            catenary_proc::kill_process(pid);
        }
    }
}

// ── Platform-specific pipe reader conversion ──────────────────────────

/// Converts an `os_pipe::PipeReader` to a tokio `AsyncRead`.
///
/// On Unix, uses `tokio::net::unix::pipe::Receiver` (epoll/kqueue).
/// On other platforms, uses `tokio::fs::File` (threadpool-backed —
/// anonymous pipes don't support overlapped I/O on Windows).
#[cfg(unix)]
fn to_async_reader(
    pipe: os_pipe::PipeReader,
) -> std::io::Result<impl AsyncRead + Unpin + Send + 'static> {
    use std::os::fd::OwnedFd;
    let fd: OwnedFd = pipe.into();
    tokio::net::unix::pipe::Receiver::from_owned_fd(fd)
}

/// Converts an `os_pipe::PipeReader` to a tokio `AsyncRead`.
///
/// Threadpool-backed: each `read()` dispatches to a blocking thread.
/// This matches how `tokio::process::ChildStdout` works on Windows
/// (anonymous pipes don't support overlapped I/O / IOCP).
#[cfg(not(unix))]
fn to_async_reader(
    pipe: os_pipe::PipeReader,
) -> std::io::Result<impl AsyncRead + Unpin + Send + 'static> {
    let file: std::fs::File = pipe.into();
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::logging::LoggingServer;
    use crate::logging::test_support::{query_all_messages, setup_logging};
    use std::time::Duration;

    #[test]
    fn lsp_response_error_display_is_byte_identical_to_the_prior_message() {
        // The typed error must render exactly the old inline `anyhow!` string so
        // logs and any message-matching stay unchanged (tui-rework 13 class B).
        let err = LspResponseError {
            code: -32601,
            language: "sql".to_string(),
            message: "Unhandled method textDocument/documentSymbol".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "[sql] LSP error -32601: Unhandled method textDocument/documentSymbol"
        );
    }

    #[test]
    fn lsp_response_error_downcasts_through_anyhow() {
        // The health probe distinguishes an error *response* (server alive) from a
        // transport failure by downcasting; a plain `anyhow!` transport error must
        // NOT match, while an `LspResponseError` (via `.into()`) must.
        let response_err: anyhow::Error = LspResponseError {
            code: -32601,
            language: "cmake".to_string(),
            message: "Method Not Found".to_string(),
        }
        .into();
        assert!(
            response_err.is::<LspResponseError>(),
            "error response is typed"
        );
        assert_eq!(
            response_err
                .downcast_ref::<LspResponseError>()
                .map(|e| e.code),
            Some(-32601)
        );

        let transport_err = anyhow::anyhow!("[cmake] server died during 'x'");
        assert!(
            !transport_err.is::<LspResponseError>(),
            "a transport failure must NOT read as an error response"
        );
    }

    /// Verify that `emit_lsp_event` routes each tracing level to the
    /// correct macro, producing DB rows with the expected level string.
    #[test]
    fn emit_lsp_event_routes_levels_correctly() {
        let (_logging, db, _guard) = setup_logging();

        emit_lsp_event(
            tracing::Level::ERROR,
            "test-server",
            "test/error",
            None,
            None,
            None,
            "{}",
            "error msg",
        );
        emit_lsp_event(
            tracing::Level::WARN,
            "test-server",
            "test/warn",
            None,
            None,
            None,
            "{}",
            "warn msg",
        );
        emit_lsp_event(
            tracing::Level::INFO,
            "test-server",
            "test/info",
            None,
            None,
            None,
            "{}",
            "info msg",
        );
        emit_lsp_event(
            tracing::Level::DEBUG,
            "test-server",
            "test/debug",
            None,
            None,
            None,
            "{}",
            "debug msg",
        );

        let msgs = query_all_messages(&db);
        assert_eq!(msgs.len(), 4, "expected 4 events, got {}", msgs.len());

        assert_eq!(msgs[0].level, "error", "ERROR event stored as error");
        assert_eq!(msgs[0].method, "test/error");

        assert_eq!(msgs[1].level, "warn", "WARN event stored as warn");
        assert_eq!(msgs[1].method, "test/warn");

        assert_eq!(msgs[2].level, "info", "INFO event stored as info");
        assert_eq!(msgs[2].method, "test/info");

        assert_eq!(msgs[3].level, "debug", "DEBUG event stored as debug");
        assert_eq!(msgs[3].method, "test/debug");
    }

    /// Verify that `emit_lsp_event` propagates `parent_id` when present.
    #[test]
    fn emit_lsp_event_propagates_parent_id() {
        let (_logging, db, _guard) = setup_logging();

        emit_lsp_event(
            tracing::Level::INFO,
            "test-server",
            "test/method",
            Some("scope-5"),
            None,
            None,
            "{}",
            "with parent",
        );
        emit_lsp_event(
            tracing::Level::INFO,
            "test-server",
            "test/method",
            None,
            None,
            None,
            "{}",
            "no parent",
        );

        let msgs = query_all_messages(&db);
        assert_eq!(msgs.len(), 2);

        assert_eq!(
            msgs[0].parent_id.as_deref(),
            Some("scope-5"),
            "parent_id should be present"
        );
        assert_eq!(msgs[1].parent_id, None, "parent_id should be absent");
    }

    #[test]
    fn suppresses_firehose_only_for_progress_report() {
        use serde_json::json;

        // $/progress report → suppressed (lives in state.json instead).
        let report =
            json!({"params": {"token": "t", "value": {"kind": "report", "percentage": 50}}});
        assert!(suppresses_firehose("$/progress", &report));

        // begin / end → kept for forensics.
        let begin =
            json!({"params": {"token": "t", "value": {"kind": "begin", "title": "Indexing"}}});
        assert!(!suppresses_firehose("$/progress", &begin));
        let end = json!({"params": {"token": "t", "value": {"kind": "end"}}});
        assert!(!suppresses_firehose("$/progress", &end));

        // Other notifications always emit, even with a report-shaped value.
        let other = json!({"params": {"value": {"kind": "report"}}});
        assert!(!suppresses_firehose(
            "textDocument/publishDiagnostics",
            &other
        ));

        // Malformed $/progress (no kind) is not suppressed.
        let malformed = json!({"params": {"token": "t"}});
        assert!(!suppresses_firehose("$/progress", &malformed));
    }

    #[test]
    fn is_retriable_lsp_error_matches_content_modified() {
        assert!(is_retriable_lsp_error(-32801));
    }

    #[test]
    fn is_retriable_lsp_error_matches_request_cancelled() {
        assert!(is_retriable_lsp_error(-32800));
    }

    #[test]
    fn is_retriable_lsp_error_rejects_other_codes() {
        assert!(!is_retriable_lsp_error(0));
        assert!(!is_retriable_lsp_error(-32700));
        assert!(!is_retriable_lsp_error(32801));
        assert!(!is_retriable_lsp_error(32800));
    }

    #[test]
    fn cancel_notification_structure() {
        let id = RequestId::Number(42);
        let notif = Connection::cancel_notification(&id);

        assert_eq!(notif.method, "$/cancelRequest");
        assert_eq!(notif.jsonrpc, "2.0");
        assert_eq!(notif.params, serde_json::json!({"id": 42}));
    }

    /// Drop must kill the child process. Without `start_kill()` in Drop,
    /// the child is orphaned and the reader loop never sees EOF.
    #[tokio::test]
    async fn drop_kills_child_process() {
        let server = std::sync::Arc::new(super::super::server::LspServer::new(
            "test".to_string(),
            "test-server".to_string(),
            None,
        ));
        let logging = LoggingServer::new();
        let bin = crate::lsp::test_support::mockls_bin();
        let (conn, _stderr) = Connection::new(
            bin.to_str().expect("mockls path is UTF-8"),
            &["test"],
            std::process::Stdio::null(),
            None,
            &server,
            "test".to_string(),
            logging,
            "test-server",
            "",
        )
        .expect("mockls should spawn");

        let alive = conn.alive_flag();
        assert!(alive.load(Ordering::SeqCst), "should be alive before drop");

        // Drop kills the child; reader loop sees EOF and sets alive=false.
        // With the mutant (drop replaced with ()), the child is orphaned
        // and alive stays true.
        drop(conn);

        // Yield to let the reader loop process EOF.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            !alive.load(Ordering::SeqCst),
            "child should be dead after Connection drop"
        );
    }
}
