// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! MCP server implementation.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::types::{
    CancelledParams, INTERNAL_ERROR, InitializeParams, InitializeResult, METHOD_NOT_FOUND,
    Notification, Request, RequestId, Response, Root, RootsListResult, ServerCapabilities,
    ServerInfo,
};
use crate::logging::LoggingServer;

/// Map an MCP method to its tracing severity level.
///
/// `notifications/cancelled` is `info` (interesting signal).
/// Everything else (initialize, roots) is `debug` (plumbing).
fn mcp_method_level(method: &str) -> tracing::Level {
    match method {
        "notifications/cancelled" => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    }
}

/// Map from MCP request ID to its cancellation token.
type CancelMap = Arc<std::sync::Mutex<HashMap<RequestId, CancellationToken>>>;

/// MCP protocol versions this server supports (newest first).
const SUPPORTED_MCP_VERSIONS: &[&str] = &["2025-11-25", "2024-11-05"];

/// Emit an MCP protocol event at the given tracing level.
///
/// Protocol routing is by `kind` field — `MessageDbSink` matches
/// `kind in {lsp, mcp, hook}` regardless of tracing level.
/// The level controls DB `level` column and TUI filtering threshold.
fn emit_mcp_event(
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
            kind = "mcp",
            method = method,
            server = "catenary",
            client = client_name,
            parent_id = parent_id,
            scope_root = Option::<&str>::None,
            source = Option::<&str>::None,
            payload = payload,
            "{msg}"
        );
    } else if level == tracing::Level::WARN {
        crate::emit_protocol_event!(
            warn,
            kind = "mcp",
            method = method,
            server = "catenary",
            client = client_name,
            parent_id = parent_id,
            scope_root = Option::<&str>::None,
            source = Option::<&str>::None,
            payload = payload,
            "{msg}"
        );
    } else if level == tracing::Level::INFO {
        crate::emit_protocol_event!(
            info,
            kind = "mcp",
            method = method,
            server = "catenary",
            client = client_name,
            parent_id = parent_id,
            scope_root = Option::<&str>::None,
            source = Option::<&str>::None,
            payload = payload,
            "{msg}"
        );
    } else {
        crate::emit_protocol_event!(
            debug,
            kind = "mcp",
            method = method,
            server = "catenary",
            client = client_name,
            parent_id = parent_id,
            scope_root = Option::<&str>::None,
            source = Option::<&str>::None,
            payload = payload,
            "{msg}"
        );
    }
}

/// Callback invoked when MCP client info is received during initialize.
pub type ClientInfoCallback = Box<dyn Fn(&str, &str) + Send + Sync>;

/// Callback invoked when MCP roots are received or updated.
pub type RootsChangedCallback = Box<dyn Fn(Vec<Root>) -> Result<()> + Send + Sync>;

/// An MCP server implementation.
///
/// Handles the MCP protocol lifecycle (initialize, roots, ping) but
/// exposes no application-level tools. Grep and glob are served via
/// CLI commands over the IPC socket.
#[allow(
    clippy::struct_excessive_bools,
    reason = "Bools track independent server state flags"
)]
pub struct McpServer {
    initialized: bool,
    _logging: LoggingServer,
    /// Name of the connected MCP client (learned during initialize).
    client_name: String,
    on_client_info: Option<ClientInfoCallback>,
    /// Whether the client advertised any `roots` capability.
    client_has_roots: bool,
    /// Flag: should we send a `roots/list` request after this message?
    should_fetch_roots: bool,
    /// Guard: are we currently inside `fetch_roots`? Prevents recursion.
    fetching_roots: bool,
    /// Counter for outbound request IDs (server-initiated).
    next_outbound_id: i64,
    /// Callback invoked when roots change.
    on_roots_changed: Option<RootsChangedCallback>,
    /// UUID of the current exchange, set per `dispatch_message`.
    /// Used to link request and response events with the same `parent_id`.
    current_exchange_id: Option<String>,
    /// Maps in-flight MCP request IDs to their cancellation tokens.
    /// Shared with the stdin reader thread so `notifications/cancelled`
    /// can trigger cancellation while a tool call blocks the main loop.
    cancel_map: CancelMap,
    /// External signal requesting a `roots/list` poll. Set by
    /// `HookRouter` on `PreAgent` dispatch (turn boundary), cleared
    /// by this run loop after triggering `fetch_roots`.
    roots_refresh: Option<Arc<AtomicBool>>,
}

impl McpServer {
    /// Creates a new `McpServer`.
    #[must_use]
    pub fn new(logging: LoggingServer) -> Self {
        Self {
            initialized: false,
            _logging: logging,
            client_name: "unknown".to_string(),
            on_client_info: None,
            client_has_roots: false,
            should_fetch_roots: false,
            fetching_roots: false,
            next_outbound_id: 0,
            on_roots_changed: None,
            current_exchange_id: None,
            cancel_map: Arc::new(std::sync::Mutex::new(HashMap::new())),
            roots_refresh: None,
        }
    }

    /// Set a callback to be invoked when client info is received.
    #[must_use]
    pub fn on_client_info(mut self, callback: ClientInfoCallback) -> Self {
        self.on_client_info = Some(callback);
        self
    }

    /// Set a callback to be invoked when MCP roots are received or updated.
    #[must_use]
    pub fn on_roots_changed(mut self, callback: RootsChangedCallback) -> Self {
        self.on_roots_changed = Some(callback);
        self
    }

    /// Set an external flag that triggers a `roots/list` poll when set.
    ///
    /// The run loop checks this flag after each message dispatch. When
    /// set and the client advertises the `roots` capability, the flag
    /// is cleared and `fetch_roots` is triggered. Used by `HookRouter`
    /// to request a roots refresh at each turn boundary.
    #[must_use]
    pub fn on_roots_refresh(mut self, flag: Arc<AtomicBool>) -> Self {
        self.roots_refresh = Some(flag);
        self
    }

    /// Runs the MCP server, reading messages from `reader` and writing
    /// responses to `writer`.
    ///
    /// The reader/writer abstraction makes the server transport-agnostic:
    /// callers pass stdin/stdout for direct mode, or socket stream halves
    /// for daemon mode.
    ///
    /// Spawns a background reader thread so that `notifications/cancelled`
    /// can trigger cancellation of in-flight tool calls while the main
    /// loop is blocked.
    ///
    /// # Errors
    ///
    /// Returns an error if reading or writing fails.
    pub fn run<R, W>(&mut self, reader: R, mut writer: W) -> Result<()>
    where
        R: std::io::Read + Send + 'static,
        W: std::io::Write,
    {
        info!("MCP server starting");

        // Spawn a reader thread that feeds lines into a channel and
        // triggers cancellation tokens for `notifications/cancelled`.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let cancel_map = self.cancel_map.clone();
        let _reader_thread = std::thread::spawn(move || {
            Self::reader_loop(reader, &tx, &cancel_map);
        });

        while let Ok(line) = rx.recv() {
            trace!("Received: {}", line);

            // Log incoming message and extract request ID for
            // cancellation pre-registration.
            let (exchange_id, method, mcp_request_id) =
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                    let method = json
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or("response")
                        .to_string();
                    let payload = json.to_string();

                    // Mint a UUID per exchange for pair-merge.
                    let uuid = uuid::Uuid::new_v4().to_string();

                    emit_mcp_event(
                        mcp_method_level(&method),
                        &self.client_name,
                        &method,
                        Some(&uuid),
                        &payload,
                        "incoming",
                    );

                    // Extract request ID for requests (has both `id` and `method`).
                    let rid = if json.get("method").is_some() {
                        json.get("id")
                            .and_then(|v| serde_json::from_value::<RequestId>(v.clone()).ok())
                    } else {
                        None
                    };
                    (Some(uuid), method, rid)
                } else {
                    (None, String::new(), None)
                };

            self.current_exchange_id.clone_from(&exchange_id);

            // Check for turn-boundary roots refresh BEFORE dispatch.
            // This guarantees tool calls execute against current roots:
            // the current message is buffered behind the roots/list
            // response, then replayed after roots are updated.
            if self.client_has_roots
                && let Some(ref flag) = self.roots_refresh
                && flag.swap(false, Ordering::AcqRel)
            {
                self.should_fetch_roots = true;
            }

            if self.should_fetch_roots {
                let initial = Some((line, exchange_id, method));
                if let Err(e) = self.fetch_roots(&rx, &mut writer, initial) {
                    error!(
                        source = crate::source::Source::McpDispatch.as_str(),
                        "Failed to fetch roots: {}", e,
                    );
                }
                if let Some(ref rid) = mcp_request_id
                    && let Ok(mut map) = self.cancel_map.lock()
                {
                    map.remove(rid);
                }
                continue;
            }

            self.dispatch_message(&line, &mut writer, &method)?;

            // Dispatch may have set should_fetch_roots (e.g.,
            // notifications/initialized, notifications/roots/list_changed).
            // Act on it now rather than deferring to the next iteration,
            // which would block on rx.recv() and deadlock if the client
            // is waiting for the roots/list request.
            if self.should_fetch_roots
                && let Err(e) = self.fetch_roots(&rx, &mut writer, None)
            {
                error!(
                    source = crate::source::Source::McpDispatch.as_str(),
                    "Failed to fetch roots: {}", e,
                );
            }

            // Clean up the cancel token after dispatch completes.
            if let Some(ref rid) = mcp_request_id
                && let Ok(mut map) = self.cancel_map.lock()
            {
                map.remove(rid);
            }
        }

        info!("MCP server shutting down (reader closed)");
        Ok(())
    }

    /// Background thread that reads from a generic reader and feeds
    /// lines into the channel. Also detects `notifications/cancelled`
    /// and triggers the matching cancellation token from the shared
    /// `cancel_map`.
    fn reader_loop<R: std::io::Read>(
        reader: R,
        tx: &std::sync::mpsc::Sender<String>,
        cancel_map: &CancelMap,
    ) {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }

                    // Pre-register cancel tokens and trigger cancellations
                    // on the same thread to eliminate the race between
                    // "request arrives" and "cancellation arrives".
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&trimmed) {
                        if json.get("method").and_then(|m| m.as_str())
                            == Some("notifications/cancelled")
                            && json.get("id").is_none()
                        {
                            Self::trigger_cancellation(&json, cancel_map);
                        } else if json.get("id").is_some()
                            && json.get("method").is_some()
                            && let Ok(rid) = serde_json::from_value::<RequestId>(json["id"].clone())
                        {
                            // Request: pre-register a cancel token so a
                            // subsequent cancellation (possibly the very
                            // next line) can find it immediately.
                            if let Ok(mut map) = cancel_map.lock() {
                                map.entry(rid).or_insert_with(CancellationToken::new);
                            }
                        }
                    }

                    if tx.send(trimmed).is_err() {
                        break; // receiver dropped
                    }
                }
            }
        }
    }

    /// Extracts `requestId` from a `notifications/cancelled` message
    /// and triggers the matching cancellation token.
    fn trigger_cancellation(json: &serde_json::Value, cancel_map: &CancelMap) {
        let Some(params) = json.get("params") else {
            return;
        };
        let Ok(cancelled) = serde_json::from_value::<CancelledParams>(params.clone()) else {
            return;
        };
        if let Ok(map) = cancel_map.lock()
            && let Some(token) = map.get(&cancelled.request_id)
        {
            info!(
                "MCP request {:?} cancelled{}",
                cancelled.request_id,
                cancelled
                    .reason
                    .as_deref()
                    .map_or(String::new(), |r| format!(": {r}")),
            );
            token.cancel();
        }
    }

    /// Dispatches a single message line, writing any response to `writer`.
    fn dispatch_message(
        &mut self,
        line: &str,
        writer: &mut impl Write,
        method: &str,
    ) -> Result<()> {
        match self.handle_message(line) {
            Ok(Some(response)) => {
                self.write_response(&response, writer, method)?;
            }
            Ok(None) => {
                // Notification, no response needed
            }
            Err(e) => {
                warn!(
                    source = crate::source::Source::McpDispatch.as_str(),
                    method = method,
                    "MCP {method} failed: {e}"
                );
                // Try to send error response if we can parse the id
                if let Ok(req) = serde_json::from_str::<Request>(line) {
                    let response = Response::error(req.id, INTERNAL_ERROR, e.to_string());
                    self.write_response(&response, writer, method)?;
                }
            }
        }
        Ok(())
    }

    /// Serializes, broadcasts, and writes a response.
    fn write_response(
        &self,
        response: &Response,
        writer: &mut impl Write,
        method: &str,
    ) -> Result<()> {
        let response_json =
            serde_json::to_string(response).context("Failed to serialize response")?;
        trace!("Sending: {}", response_json);

        // Response carries the same parent_id as the incoming request
        // (minted per exchange in the run loop).
        if let Some(ref eid) = self.current_exchange_id {
            emit_mcp_event(
                mcp_method_level(method),
                &self.client_name,
                method,
                Some(eid),
                &response_json,
                "outgoing response",
            );
        }

        writeln!(writer, "{response_json}")?;
        writer.flush()?;
        Ok(())
    }

    fn handle_message(&mut self, line: &str) -> Result<Option<Response>> {
        // Try to parse as request first
        if let Ok(request) = serde_json::from_str::<Request>(line) {
            let response = self.handle_request(request)?;
            return Ok(Some(response));
        }

        // Try to parse as notification
        if let Ok(notification) = serde_json::from_str::<Notification>(line) {
            self.handle_notification(&notification);
            return Ok(None);
        }

        Err(anyhow!(
            "Failed to parse message as request or notification"
        ))
    }

    fn handle_request(&mut self, request: Request) -> Result<Response> {
        debug!("Handling request: {} (id={:?})", request.method, request.id);

        match request.method.as_str() {
            "initialize" => self.handle_initialize(request),
            "ping" => Ok(Response::success(request.id, serde_json::json!({}))?),
            _ => {
                debug!("Unknown method: {}", request.method);
                Ok(Response::error(
                    request.id,
                    METHOD_NOT_FOUND,
                    format!("Unknown method: {}", request.method),
                ))
            }
        }
    }

    fn handle_notification(&mut self, notification: &Notification) {
        debug!("Handling notification: {}", notification.method);

        match notification.method.as_str() {
            "notifications/initialized" => {
                info!("MCP client initialized");
                self.initialized = true;
                if self.client_has_roots {
                    self.should_fetch_roots = true;
                }
            }
            "notifications/roots/list_changed" => {
                info!("MCP client roots changed");
                // Always honor — the client explicitly told us roots changed,
                // regardless of what it advertised during initialization.
                self.should_fetch_roots = true;
            }
            "notifications/cancelled" => {
                // Cancellation is handled proactively by the reader
                // thread. If we see it here, the request already finished.
                debug!("notifications/cancelled received (request already complete)");
            }
            "catenary/version-mismatch" => {
                let bridge_version = notification
                    .params
                    .as_ref()
                    .and_then(|p| p.get("bridgeVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                warn!(
                    source = crate::source::Source::DaemonLifecycle.as_str(),
                    daemon_version = env!("CATENARY_VERSION"),
                    bridge_version = bridge_version,
                    "Version mismatch: bridge v{bridge_version} rejected \
                     (daemon is v{})",
                    env!("CATENARY_VERSION"),
                );
            }
            _ => {
                debug!("Ignoring unknown notification: {}", notification.method);
            }
        }
    }

    fn handle_initialize(&mut self, request: Request) -> Result<Response> {
        let params: InitializeParams = request
            .params
            .map(serde_json::from_value)
            .transpose()
            .context("Invalid initialize params")?
            .ok_or_else(|| anyhow!("Missing initialize params"))?;

        self.client_name.clone_from(&params.client_info.name);
        let client_name = &params.client_info.name;
        let client_version = params.client_info.version.as_deref().unwrap_or("unknown");

        info!("MCP client connecting: {} v{}", client_name, client_version);
        info!("Protocol version requested: {}", params.protocol_version);

        // Negotiate protocol version per MCP spec: echo the requested
        // version if we support it, otherwise respond with our latest.
        let negotiated_version =
            if SUPPORTED_MCP_VERSIONS.contains(&params.protocol_version.as_str()) {
                params.protocol_version.clone()
            } else {
                info!(
                    "Unsupported protocol version '{}', responding with {}",
                    params.protocol_version, SUPPORTED_MCP_VERSIONS[0]
                );
                SUPPORTED_MCP_VERSIONS[0].to_string()
            };

        // Store whether client supports roots
        self.client_has_roots = params.capabilities.roots.is_some();

        if self.client_has_roots {
            info!("Client supports roots capability");
        }

        // Notify callback of client info
        if let Some(ref callback) = self.on_client_info {
            callback(client_name, client_version);
        }

        let result = InitializeResult {
            protocol_version: negotiated_version,
            capabilities: ServerCapabilities { tools: None },
            server_info: ServerInfo {
                name: "catenary".to_string(),
                version: Some(env!("CATENARY_VERSION").to_string()),
            },
            instructions: None,
        };

        Ok(Response::success(request.id, result)?)
    }

    /// Generates a unique request ID for server-initiated requests.
    fn next_id(&mut self) -> RequestId {
        let id = self.next_outbound_id;
        self.next_outbound_id += 1;
        RequestId::String(format!("catenary-{id}"))
    }

    /// Sends a `roots/list` request to the client and processes the response.
    ///
    /// Handles interleaved client requests/notifications while waiting for
    /// the response. Uses `fetching_roots` guard to prevent recursion if
    /// `roots/list_changed` arrives during the fetch.
    ///
    /// `initial_message` is an already-read message (line, exchange UUID,
    /// method) that should be buffered behind the roots response. Used by
    /// the turn-boundary refresh path to ensure tool calls execute against
    /// updated roots.
    fn fetch_roots(
        &mut self,
        inbox: &std::sync::mpsc::Receiver<String>,
        writer: &mut impl Write,
        initial_message: Option<(String, Option<String>, String)>,
    ) -> Result<()> {
        if self.fetching_roots {
            debug!("Already fetching roots, skipping");
            return Ok(());
        }
        self.fetching_roots = true;
        self.should_fetch_roots = false;

        let result = self.fetch_roots_inner(inbox, writer, initial_message);
        self.fetching_roots = false;
        result
    }

    /// Inner implementation of [`Self::fetch_roots`].
    fn fetch_roots_inner(
        &mut self,
        inbox: &std::sync::mpsc::Receiver<String>,
        writer: &mut impl Write,
        initial_message: Option<(String, Option<String>, String)>,
    ) -> Result<()> {
        let request_id = self.next_id();
        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: request_id.clone(),
            method: "roots/list".to_string(),
            params: None,
        };

        let request_json =
            serde_json::to_string(&request).context("Failed to serialize roots/list request")?;
        trace!("Sending roots/list request: {}", request_json);

        // Log outbound request — mint a UUID for this exchange
        let outbound_uuid = uuid::Uuid::new_v4().to_string();
        if let Ok(json) = serde_json::to_value(&request) {
            emit_mcp_event(
                mcp_method_level("roots/list"),
                &self.client_name,
                "roots/list",
                Some(&outbound_uuid),
                &json.to_string(),
                "outgoing request",
            );
        }

        writeln!(writer, "{request_json}")?;
        writer.flush()?;

        // Read lines until we get the matching response.
        // Buffer interleaved requests (id + method) until roots are applied,
        // so they execute against the updated PathValidator.
        // Notifications are dispatched immediately.
        let mut buffered: Vec<(String, Option<String>, String)> = Vec::new();

        // Seed with the already-read message from the run loop (if any).
        // Already logged by the run loop, so buffer or dispatch without
        // re-logging. Virtually always a request (tool call) that gets
        // buffered; notifications are dispatched immediately.
        if let Some((line, eid, method)) = initial_message {
            let json: serde_json::Value =
                serde_json::from_str(&line).context("Failed to parse initial message")?;
            if json.get("id").is_some() && json.get("method").is_some() {
                buffered.push((line, eid, method));
            } else {
                self.current_exchange_id = eid;
                self.dispatch_message(&line, writer, &method)?;
            }
        }

        loop {
            let trimmed = inbox
                .recv()
                .map_err(|_| anyhow!("stdin closed while waiting for roots/list response"))?;

            trace!("Received (during roots/list wait): {}", trimmed);

            // Parse JSON once for disambiguation and logging
            let json: serde_json::Value = serde_json::from_str(&trimmed)
                .context("Failed to parse JSON during roots/list wait")?;

            // Response: has `id` + no `method` + (`result` or `error`)
            let is_response = json.get("id").is_some()
                && json.get("method").is_none()
                && (json.get("result").is_some() || json.get("error").is_some());

            if is_response {
                let response: Response =
                    serde_json::from_value(json).context("Failed to parse roots/list response")?;
                if response.id == request_id {
                    // Log the response — same parent_id as outbound request
                    if let Ok(resp_json) = serde_json::to_value(&response) {
                        emit_mcp_event(
                            mcp_method_level("roots/list"),
                            &self.client_name,
                            "roots/list",
                            Some(&outbound_uuid),
                            &resp_json.to_string(),
                            "incoming response",
                        );
                    }
                    let result = self.handle_roots_response(response);
                    // Replay buffered requests against the updated roots
                    for (msg, buf_eid, buf_method) in &buffered {
                        self.current_exchange_id.clone_from(buf_eid);
                        self.dispatch_message(msg, writer, buf_method)?;
                    }
                    return result;
                }
                debug!(
                    "Received response with unexpected ID {:?} while waiting for roots/list",
                    response.id
                );
                continue;
            }

            // Non-response: log the incoming message, then buffer or dispatch.
            let method = json
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("response")
                .to_string();
            let interleaved_uuid = uuid::Uuid::new_v4().to_string();
            emit_mcp_event(
                mcp_method_level(&method),
                &self.client_name,
                &method,
                Some(&interleaved_uuid),
                &json.to_string(),
                "incoming",
            );

            // Requests (id + method) are buffered until roots are applied.
            // Notifications dispatch immediately.
            if json.get("id").is_some() && json.get("method").is_some() {
                buffered.push((trimmed, Some(interleaved_uuid), method));
            } else {
                self.current_exchange_id = Some(interleaved_uuid);
                self.dispatch_message(&trimmed, writer, &method)?;
            }
        }
    }

    /// Processes the response to a `roots/list` request.
    fn handle_roots_response(&self, response: Response) -> Result<()> {
        if let Some(error) = response.error {
            warn!(
                source = crate::source::Source::McpDispatch.as_str(),
                "roots/list request failed: {} (code {})", error.message, error.code,
            );
            return Ok(()); // Non-fatal
        }

        let result_value = response
            .result
            .ok_or_else(|| anyhow!("roots/list response has neither result nor error"))?;

        let roots_result: RootsListResult =
            serde_json::from_value(result_value).context("Failed to parse roots/list result")?;

        info!(
            "Received {} root(s) from MCP client",
            roots_result.roots.len()
        );
        for root in &roots_result.roots {
            info!(
                "  Root: {} ({})",
                root.uri,
                root.name.as_deref().unwrap_or("unnamed")
            );
        }

        if let Some(ref callback) = self.on_roots_changed
            && let Err(e) = callback(roots_result.roots)
        {
            error!(
                source = crate::source::Source::McpDispatch.as_str(),
                "Failed to apply roots: {}", e,
            );
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_initialize() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "test-client",
                    "version": "1.0.0"
                }
            })),
        };

        let response = server.handle_request(request)?;
        assert!(response.result.is_some());
        assert!(response.error.is_none());

        let result: InitializeResult =
            serde_json::from_value(response.result.expect("response result"))?;
        assert_eq!(result.server_info.name, "catenary");
        assert_eq!(result.protocol_version, "2024-11-05");
        assert!(result.instructions.is_none());
        Ok(())
    }

    #[test]
    fn test_tools_list_returns_method_not_found() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(2),
            method: "tools/list".to_string(),
            params: None,
        };

        let response = server.handle_request(request)?;
        assert!(response.error.is_some());
        assert_eq!(
            response.error.expect("response error").code,
            METHOD_NOT_FOUND
        );
        Ok(())
    }

    #[test]
    fn test_tools_call_returns_method_not_found() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(3),
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({
                "name": "test_tool",
                "arguments": {}
            })),
        };

        let response = server.handle_request(request)?;
        assert!(response.error.is_some());
        assert_eq!(
            response.error.expect("response error").code,
            METHOD_NOT_FOUND
        );
        Ok(())
    }

    #[test]
    fn test_handle_unknown_method() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(5),
            method: "unknown/method".to_string(),
            params: None,
        };

        let response = server.handle_request(request)?;
        assert!(response.error.is_some());
        assert_eq!(
            response.error.expect("response error").code,
            METHOD_NOT_FOUND
        );
        Ok(())
    }

    #[test]
    fn test_handle_ping() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(6),
            method: "ping".to_string(),
            params: None,
        };

        let response = server.handle_request(request)?;
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        Ok(())
    }

    fn initialize_server(server: &mut McpServer, with_roots: bool) -> Result<()> {
        let caps = if with_roots {
            serde_json::json!({"roots": {"listChanged": true}})
        } else {
            serde_json::json!({})
        };

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(99),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": caps,
                "clientInfo": {"name": "test", "version": "1.0"}
            })),
        };
        let _ = server.handle_request(request)?;
        Ok(())
    }

    #[test]
    fn test_roots_capability_stored_when_present() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        assert!(!server.client_has_roots);

        initialize_server(&mut server, true)?;
        assert!(server.client_has_roots);
        Ok(())
    }

    #[test]
    fn test_roots_capability_absent_by_default() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, false)?;
        assert!(!server.client_has_roots);
        Ok(())
    }

    #[test]
    fn test_should_fetch_roots_after_initialized() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(server.should_fetch_roots);
        assert!(server.initialized);
        Ok(())
    }

    #[test]
    fn test_should_fetch_roots_on_list_changed() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/roots/list_changed".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(server.should_fetch_roots);
        Ok(())
    }

    #[test]
    fn test_no_fetch_without_capability() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, false)?;

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(!server.should_fetch_roots);
        Ok(())
    }

    // ── Cancellation tests ───────────────────────────────────────────

    #[test]
    fn test_cancelled_notification_triggers_token() {
        let cancel_map: CancelMap = Arc::new(std::sync::Mutex::new(HashMap::new()));

        let token = CancellationToken::new();
        cancel_map
            .lock()
            .expect("lock")
            .insert(RequestId::Number(42), token.clone());

        let json = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 42}
        });

        assert!(!token.is_cancelled());
        McpServer::trigger_cancellation(&json, &cancel_map);
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_cancelled_notification_no_match_is_noop() {
        let cancel_map: CancelMap = Arc::new(std::sync::Mutex::new(HashMap::new()));

        let token = CancellationToken::new();
        cancel_map
            .lock()
            .expect("lock")
            .insert(RequestId::Number(42), token.clone());

        // Cancel a different request ID — should not trigger our token.
        let json = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 99}
        });

        McpServer::trigger_cancellation(&json, &cancel_map);
        assert!(!token.is_cancelled());
    }

    /// Creates a channel pre-loaded with JSON messages, simulating stdin.
    fn mock_inbox(messages: &[serde_json::Value]) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        for msg in messages {
            tx.send(serde_json::to_string(msg).expect("serialize"))
                .expect("send");
        }
        drop(tx); // close after all messages sent
        rx
    }

    #[test]
    fn test_fetch_roots_parses_response() -> Result<()> {
        use std::sync::{Arc, Mutex};

        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        let received_roots: Arc<Mutex<Vec<Root>>> = Arc::new(Mutex::new(Vec::new()));
        let roots_clone = received_roots.clone();
        server.on_roots_changed = Some(Box::new(move |roots| {
            if let Ok(mut guard) = roots_clone.lock() {
                *guard = roots;
            }
            Ok(())
        }));

        server.should_fetch_roots = true;

        // Mock stdin: the response to our roots/list request
        let response_json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "result": {
                "roots": [
                    {"uri": "file:///tmp/project_a", "name": "Project A"},
                    {"uri": "file:///tmp/project_b"}
                ]
            }
        });
        let inbox = mock_inbox(&[response_json]);
        let mut writer: Vec<u8> = Vec::new();

        server.fetch_roots(&inbox, &mut writer, None)?;

        let roots = received_roots.lock().map_err(|e| anyhow!("{e}"))?;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].uri, "file:///tmp/project_a");
        assert_eq!(roots[0].name.as_deref(), Some("Project A"));
        assert_eq!(roots[1].uri, "file:///tmp/project_b");
        assert!(roots[1].name.is_none());
        drop(roots);

        // Verify the outbound request was written
        let output = String::from_utf8(writer)?;
        assert!(output.contains("roots/list"));
        assert!(output.contains("catenary-0"));
        Ok(())
    }

    #[test]
    fn test_fetch_roots_buffers_interleaved_request() -> Result<()> {
        use std::sync::{Arc, Mutex};

        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        let received_roots: Arc<Mutex<Vec<Root>>> = Arc::new(Mutex::new(Vec::new()));
        let roots_clone = received_roots.clone();
        server.on_roots_changed = Some(Box::new(move |roots| {
            if let Ok(mut guard) = roots_clone.lock() {
                *guard = roots;
            }
            Ok(())
        }));

        server.should_fetch_roots = true;

        // Mock stdin: a ping request arrives BEFORE the roots/list response.
        // The request should be buffered and replayed after roots are applied.
        let ping_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "ping"
        });
        let roots_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "result": {"roots": [{"uri": "file:///tmp/test"}]}
        });
        let inbox = mock_inbox(&[ping_request, roots_response]);
        let mut writer: Vec<u8> = Vec::new();

        server.fetch_roots(&inbox, &mut writer, None)?;

        // Verify roots were received
        let roots = received_roots.lock().map_err(|e| anyhow!("{e}"))?;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].uri, "file:///tmp/test");
        drop(roots);

        // Verify both the roots/list request AND the ping response were written,
        // and that the ping response (buffered) appears after the roots/list request.
        let output = String::from_utf8(writer)?;
        let roots_pos = output
            .find("roots/list")
            .ok_or_else(|| anyhow!("roots/list request not found in output"))?;
        let ping_pos = output
            .find(r#""id":42"#)
            .ok_or_else(|| anyhow!("ping response not found in output"))?;
        assert!(
            roots_pos < ping_pos,
            "ping response should appear after roots/list request (buffered)"
        );
        Ok(())
    }

    #[test]
    fn test_fetch_roots_handles_error_response() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;
        server.should_fetch_roots = true;

        // Mock stdin: an error response
        let error_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "error": {"code": -32601, "message": "roots/list not supported"}
        });
        let inbox = mock_inbox(&[error_response]);
        let mut writer: Vec<u8> = Vec::new();

        // Should not error — error responses are non-fatal
        server.fetch_roots(&inbox, &mut writer, None)?;
        assert!(!server.fetching_roots);
        Ok(())
    }

    #[test]
    fn test_cancelled_notification_handled_without_panic() {
        // notifications/cancelled arriving after the tool call completed
        // should be silently accepted (not fall through to unknown).
        let mut server = McpServer::new(LoggingServer::new());

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/cancelled".to_string(),
            params: Some(serde_json::json!({"requestId": 99})),
        };
        // Should not set should_fetch_roots or any other flag — it's a no-op.
        server.handle_notification(&notification);
        assert!(
            !server.should_fetch_roots,
            "cancelled should not trigger roots fetch"
        );
    }

    #[test]
    fn test_list_changed_honored_without_capability() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        // Initialize WITHOUT roots capability
        initialize_server(&mut server, false)?;
        assert!(!server.client_has_roots);

        // Client sends roots/list_changed anyway — we must honor it
        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/roots/list_changed".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(server.should_fetch_roots);
        Ok(())
    }

    #[test]
    fn test_roots_capability_without_list_changed() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());

        // Initialize with `roots: {}` (no listChanged field)
        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(99),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"roots": {}},
                "clientInfo": {"name": "test", "version": "1.0"}
            })),
        };
        let _ = server.handle_request(request)?;

        // roots.is_some() should be true even without listChanged
        assert!(server.client_has_roots);
        Ok(())
    }

    #[test]
    fn test_fetching_roots_reset_on_error() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;
        server.should_fetch_roots = true;

        // Empty channel — will cause recv error during fetch
        let inbox = mock_inbox(&[]);
        let mut writer: Vec<u8> = Vec::new();

        let result = server.fetch_roots(&inbox, &mut writer, None);
        assert!(result.is_err());
        // fetching_roots must be reset even on error
        assert!(!server.fetching_roots);
        Ok(())
    }

    // ── Protocol logging integration tests ────────────────────────────

    use crate::logging::test_support::{MsgRow, query_all_messages, setup_logging};

    /// Filter to MCP protocol rows only.
    fn mcp_messages(conn: &Arc<std::sync::Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
        query_all_messages(conn)
            .into_iter()
            .filter(|m| m.r#type == "mcp")
            .collect()
    }

    /// Simulate the `run()` loop for a single message: mint a UUID,
    /// emit the incoming MCP event, set `current_exchange_id`, and
    /// dispatch. Returns the exchange UUID.
    fn simulate_incoming(
        server: &mut McpServer,
        line: &str,
        writer: &mut Vec<u8>,
    ) -> Result<String> {
        let json: serde_json::Value = serde_json::from_str(line).context("invalid JSON in test")?;
        let method = json
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("response")
            .to_string();
        let payload = json.to_string();
        let uuid = uuid::Uuid::new_v4().to_string();
        emit_mcp_event(
            mcp_method_level(&method),
            &server.client_name,
            &method,
            Some(&uuid),
            &payload,
            "incoming",
        );
        server.current_exchange_id = Some(uuid.clone());
        server.dispatch_message(line, writer, &method)?;
        Ok(uuid)
    }

    #[test]
    fn test_mcp_log_initialize() -> Result<()> {
        let (logging, conn, _guard) = setup_logging();
        let mut server = McpServer::new(logging);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "1.0.0"}
            })),
        };

        let line = serde_json::to_string(&request)?;
        let mut writer: Vec<u8> = Vec::new();
        let exchange_id = simulate_incoming(&mut server, &line, &mut writer)?;

        let msgs = mcp_messages(&conn);
        assert!(
            msgs.len() >= 2,
            "should have at least request + response, got {}",
            msgs.len()
        );
        assert_eq!(msgs[0].method, "initialize");
        assert_eq!(
            msgs[0].parent_id.as_deref(),
            Some(exchange_id.as_str()),
            "request should carry exchange UUID"
        );
        assert_eq!(msgs[1].method, "initialize");
        assert_eq!(
            msgs[1].parent_id.as_deref(),
            Some(exchange_id.as_str()),
            "response should carry same exchange UUID"
        );
        Ok(())
    }

    #[test]
    fn test_mcp_log_notification() -> Result<()> {
        let (logging, conn, _guard) = setup_logging();
        let mut server = McpServer::new(logging);

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };

        let line = serde_json::to_string(&notification)?;
        let mut writer: Vec<u8> = Vec::new();
        simulate_incoming(&mut server, &line, &mut writer)?;

        let msgs = mcp_messages(&conn);
        assert!(
            !msgs.is_empty(),
            "should have at least the notification row"
        );
        assert_eq!(msgs[0].method, "notifications/initialized");
        assert!(msgs[0].parent_id.is_some(), "should have an exchange UUID");
        Ok(())
    }

    #[test]
    fn test_mcp_log_client_name() -> Result<()> {
        let (logging, conn, _guard) = setup_logging();
        let mut server = McpServer::new(logging);

        // Initialize to set client_name
        let init_request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "claude-code", "version": "2.0.0"}
            })),
        };

        let line = serde_json::to_string(&init_request)?;
        let mut writer: Vec<u8> = Vec::new();
        simulate_incoming(&mut server, &line, &mut writer)?;

        // Now send a second request — client_name should be "claude-code"
        let ping = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(2),
            method: "ping".to_string(),
            params: None,
        };

        let line = serde_json::to_string(&ping)?;
        simulate_incoming(&mut server, &line, &mut writer)?;

        let msgs = mcp_messages(&conn);
        // MCP rows: init req, init resp, ping req, ping resp = 4
        assert!(
            msgs.len() >= 4,
            "should have at least init pair + ping pair, got {}",
            msgs.len()
        );
        // The ping request (3rd MCP message) should have client = "claude-code"
        assert_eq!(msgs[2].client, "claude-code");
        // The ping response (4th MCP message) should also have client = "claude-code"
        assert_eq!(msgs[3].client, "claude-code");
        Ok(())
    }

    // ── Level-aware emit tests ──────────────────────────────────────

    #[test]
    fn test_mcp_initialize_emits_at_debug() -> Result<()> {
        let (logging, conn, _guard) = setup_logging();
        let mut server = McpServer::new(logging);

        let request = Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(11),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            })),
        };

        let line = serde_json::to_string(&request)?;
        let mut writer: Vec<u8> = Vec::new();
        simulate_incoming(&mut server, &line, &mut writer)?;

        let msgs = mcp_messages(&conn);
        assert!(msgs.len() >= 2, "should have request + response");
        assert_eq!(msgs[0].level, "debug", "initialize request should be debug");
        assert_eq!(
            msgs[1].level, "debug",
            "initialize response should be debug"
        );
        Ok(())
    }

    // ── Turn-boundary roots refresh tests ────────────────────────────

    #[test]
    fn test_roots_refresh_flag_triggers_fetch() -> Result<()> {
        use std::sync::{Arc, Mutex};

        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        // Simulate PreAgent setting the external flag.
        let flag = Arc::new(AtomicBool::new(true));
        server.roots_refresh = Some(flag.clone());

        let received_roots: Arc<Mutex<Vec<Root>>> = Arc::new(Mutex::new(Vec::new()));
        let roots_clone = received_roots.clone();
        server.on_roots_changed = Some(Box::new(move |roots| {
            if let Ok(mut guard) = roots_clone.lock() {
                *guard = roots;
            }
            Ok(())
        }));

        // Manually simulate what `run()` does: check the flag BEFORE
        // dispatch, then call `fetch_roots` with the current message
        // as initial_message so it's buffered behind the roots response.
        assert!(!server.should_fetch_roots);
        if server.client_has_roots
            && let Some(ref f) = server.roots_refresh
            && f.swap(false, Ordering::AcqRel)
        {
            server.should_fetch_roots = true;
        }
        assert!(server.should_fetch_roots, "flag should trigger fetch");
        assert!(!flag.load(Ordering::Acquire), "flag should be cleared");

        // Simulate: a tool call arrives, but fetch_roots runs first
        // with the tool call as initial_message. The roots/list response
        // is in the inbox. The tool call should be buffered and replayed
        // AFTER roots are updated.
        let response_json = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "result": {"roots": [{"uri": "file:///tmp/refreshed"}]}
        });
        let inbox = mock_inbox(&[response_json]);
        let mut writer: Vec<u8> = Vec::new();

        // The initial message is a ping (simulating a tool call).
        let initial = Some((
            r#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#.to_string(),
            Some("exchange-100".to_string()),
            "ping".to_string(),
        ));
        server.fetch_roots(&inbox, &mut writer, initial)?;

        // Roots should be updated.
        let roots = received_roots.lock().map_err(|e| anyhow!("{e}"))?;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].uri, "file:///tmp/refreshed");
        drop(roots);

        // The ping should have been replayed AFTER roots were applied.
        // Verify: roots/list request appears first, then ping response.
        let output = String::from_utf8(writer)?;
        let roots_pos = output
            .find("roots/list")
            .ok_or_else(|| anyhow!("roots/list not found"))?;
        let ping_pos = output
            .find(r#""id":42"#)
            .ok_or_else(|| anyhow!("ping response not found"))?;
        assert!(
            roots_pos < ping_pos,
            "ping response should appear after roots/list (was buffered)"
        );
        Ok(())
    }

    #[test]
    fn test_roots_refresh_skipped_without_capability() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        // Initialize WITHOUT roots capability.
        initialize_server(&mut server, false)?;

        let flag = Arc::new(AtomicBool::new(true));
        server.roots_refresh = Some(flag.clone());

        // Check the flag — should NOT trigger because client_has_roots is false.
        if server.client_has_roots
            && let Some(ref f) = server.roots_refresh
            && f.swap(false, Ordering::AcqRel)
        {
            server.should_fetch_roots = true;
        }
        assert!(
            !server.should_fetch_roots,
            "should not fetch without roots capability"
        );
        // Flag stays set (was never consumed).
        assert!(
            flag.load(Ordering::Acquire),
            "flag should remain set when capability is absent"
        );
        Ok(())
    }

    #[test]
    fn test_roots_refresh_noop_when_not_set() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        // Flag exists but is false.
        let flag = Arc::new(AtomicBool::new(false));
        server.roots_refresh = Some(flag);

        if server.client_has_roots
            && let Some(ref f) = server.roots_refresh
            && f.swap(false, Ordering::AcqRel)
        {
            server.should_fetch_roots = true;
        }
        assert!(
            !server.should_fetch_roots,
            "should not fetch when flag is not set"
        );
        Ok(())
    }

    #[test]
    fn test_roots_refresh_without_external_flag() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        // No external flag wired (roots_refresh is None).
        assert!(server.roots_refresh.is_none());

        if server.client_has_roots
            && let Some(ref f) = server.roots_refresh
            && f.swap(false, Ordering::AcqRel)
        {
            server.should_fetch_roots = true;
        }
        assert!(
            !server.should_fetch_roots,
            "should not fetch when no external flag is wired"
        );
        Ok(())
    }

    // ── Version mismatch notification tests ────────────────────────

    #[test]
    fn mismatch_emits_warning() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::SubscriberExt;

        struct WarnCapture {
            sources: Arc<Mutex<Vec<String>>>,
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnCapture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if *event.metadata().level() == tracing::Level::WARN {
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
                        && let Ok(mut w) = self.sources.lock()
                    {
                        w.push(src);
                    }
                }
            }
        }

        let sources = Arc::new(Mutex::new(Vec::new()));
        let layer = WarnCapture {
            sources: Arc::clone(&sources),
        };

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut server = McpServer::new(LoggingServer::new());

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "catenary/version-mismatch".to_string(),
            params: Some(serde_json::json!({"bridgeVersion": "0.0.0-fake"})),
        };
        server.handle_notification(&notification);

        let captured = sources.lock().expect("lock").clone();
        assert!(
            captured.contains(&"daemon.lifecycle".to_string()),
            "should emit daemon.lifecycle warning, got: {captured:?}",
        );
    }

    #[test]
    fn test_list_changed_and_refresh_coexist() -> Result<()> {
        use std::sync::{Arc, Mutex};

        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        let flag = Arc::new(AtomicBool::new(false));
        server.roots_refresh = Some(flag.clone());

        let received_roots: Arc<Mutex<Vec<Root>>> = Arc::new(Mutex::new(Vec::new()));
        let roots_clone = received_roots.clone();
        server.on_roots_changed = Some(Box::new(move |roots| {
            if let Ok(mut guard) = roots_clone.lock() {
                *guard = roots;
            }
            Ok(())
        }));

        // list_changed fires first — sets should_fetch_roots directly.
        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/roots/list_changed".to_string(),
            params: None,
        };
        server.handle_notification(&notification);
        assert!(server.should_fetch_roots);

        // Fetch roots via list_changed.
        let response1 = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-0",
            "result": {"roots": [{"uri": "file:///tmp/via_list_changed"}]}
        });
        let inbox = mock_inbox(&[response1]);
        let mut writer: Vec<u8> = Vec::new();
        server.fetch_roots(&inbox, &mut writer, None)?;

        let roots = received_roots.lock().map_err(|e| anyhow!("{e}"))?;
        assert_eq!(roots[0].uri, "file:///tmp/via_list_changed");
        drop(roots);

        // Now turn-boundary flag fires.
        flag.store(true, Ordering::Release);
        if server.client_has_roots
            && let Some(ref f) = server.roots_refresh
            && f.swap(false, Ordering::AcqRel)
        {
            server.should_fetch_roots = true;
        }
        assert!(server.should_fetch_roots);

        // Fetch roots via turn-boundary poll.
        let response2 = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catenary-1",
            "result": {"roots": [{"uri": "file:///tmp/via_turn_boundary"}]}
        });
        let inbox = mock_inbox(&[response2]);
        writer.clear();
        server.fetch_roots(&inbox, &mut writer, None)?;

        let roots = received_roots.lock().map_err(|e| anyhow!("{e}"))?;
        assert_eq!(roots[0].uri, "file:///tmp/via_turn_boundary");
        drop(roots);
        Ok(())
    }

    // ── run() loop tests ────────────────────────────────────────────

    /// Build newline-delimited JSON bytes from a slice of messages,
    /// suitable as synthetic stdin for `run()`.
    fn synthetic_stdin(messages: &[serde_json::Value]) -> std::io::Cursor<Vec<u8>> {
        let mut buf = Vec::new();
        for msg in messages {
            buf.extend_from_slice(serde_json::to_string(msg).expect("serialize").as_bytes());
            buf.push(b'\n');
        }
        std::io::Cursor::new(buf)
    }

    #[test]
    fn reader_loop_cancellation_requires_notification() {
        // A message with method "notifications/cancelled" AND an "id"
        // field is a malformed request, not a notification. The reader
        // loop must NOT trigger cancellation for it.
        let cancel_map: CancelMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let token = CancellationToken::new();
        cancel_map
            .lock()
            .expect("lock")
            .insert(RequestId::Number(42), token.clone());

        // Malformed: has both "method": "notifications/cancelled" AND "id".
        let bad_line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "notifications/cancelled",
            "params": {"requestId": 42}
        });

        let stdin = synthetic_stdin(&[bad_line]);
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        McpServer::reader_loop(stdin, &tx, &cancel_map);

        assert!(
            !token.is_cancelled(),
            "should NOT trigger cancellation when id is present (request, not notification)"
        );
    }
}
