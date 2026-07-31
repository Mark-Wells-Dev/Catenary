// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! MCP server implementation.
//!
//! The roots channel has exactly two triggers: a roots-capable
//! `initialize` arms the fetch itself (misc 169 — replay-safe, so a bridge
//! reattaching to a fresh daemon re-anchors its roots), and a client-pushed
//! `notifications/roots/list_changed` arms it again on every announced
//! change. Both are message-driven; nothing polls this run loop from
//! outside it.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use crate::logging::LoggingServer;
use catenary_mcp::protocol::{
    BRIDGE_HELLO_METHOD, BridgeHelloParams, CancelledParams, INTERNAL_ERROR, InitializeParams,
    InitializeResult, METHOD_NOT_FOUND, Notification, Request, RequestId, Response, Root,
    RootsListResult, ServerCapabilities, ServerInfo,
};

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

/// The ws41-01-generation bridge's mismatch notification (legacy compat).
///
/// That generation compares versions bridge-side (its git-describe
/// `CATENARY_VERSION` against the daemon's `serverInfo.version`), sends this
/// notification on disagreement, and then bails — tearing its own session down.
/// It predates the [`BRIDGE_HELLO_METHOD`] hello, so on receipt the daemon
/// treats it exactly like an absent hello: a pre-handshake bridge, cured by
/// `/mcp`. Kept here (not in the wire-definition crate) because it is a
/// receive-only compat shim for a retired message, never sent by current code.
const LEGACY_VERSION_MISMATCH_METHOD: &str = "catenary/version-mismatch";

/// Emit an MCP protocol event at the given tracing level.
///
/// Protocol routing is by `kind` field — the JSONL firehose sink keys
/// `kind in {lsp, mcp, hook}` regardless of tracing level. The level sets
/// the record's `level` and the TUI filtering threshold.
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

/// Callback invoked when the bridge-hello question resolves (ws41-02).
///
/// The argument is the bridge's reported `catenary-mcp` version, or `None` for
/// a pre-handshake bridge — one whose hello never arrived by the first
/// post-`initialize` message ([`McpServer::check_bridge_hello_absent`]), or a
/// ws41-01-generation bridge announcing itself via the legacy mismatch
/// notification. The daemon-side wiring compares it against its linked version,
/// records/clears the mismatch on the snapshot, and fires the once-per-pairing
/// interrupt.
pub type BridgeHelloCallback = Box<dyn Fn(Option<&str>) + Send + Sync>;

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
    /// Callback invoked when a bridge hello arrives (ws41-02), carrying the
    /// bridge's reported version for the daemon-side comparison/surfacing.
    on_bridge_hello: Option<BridgeHelloCallback>,
    /// Whether `initialize` has been dispatched — the anchor for the
    /// absent-hello check (ws41-02). A hello-capable bridge injects its hello
    /// immediately after forwarding `initialize` on the same in-order pipe, so
    /// only messages *after* `initialize` can prove a hello absent.
    init_seen: bool,
    /// Whether the bridge-hello question is resolved (ws41-02): a hello (or the
    /// legacy ws41-01 mismatch notification) arrived, or the absent-hello
    /// `None` already fired. Guards the absent case to exactly one callback
    /// invocation per connection.
    hello_checked: bool,
    /// Whether the client advertised any `roots` capability.
    client_has_roots: bool,
    /// Flag: should we send a `roots/list` request after this message?
    ///
    /// Two triggers arm it: a roots-capable `initialize` (misc 169,
    /// replay-safe) and a client-pushed `notifications/roots/list_changed`.
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
            on_bridge_hello: None,
            init_seen: false,
            hello_checked: false,
            client_has_roots: false,
            should_fetch_roots: false,
            fetching_roots: false,
            next_outbound_id: 0,
            on_roots_changed: None,
            current_exchange_id: None,
            cancel_map: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Set a callback to be invoked when client info is received.
    #[must_use]
    pub fn on_client_info(mut self, callback: ClientInfoCallback) -> Self {
        self.on_client_info = Some(callback);
        self
    }

    /// Set a callback to be invoked when a bridge hello arrives (ws41-02).
    #[must_use]
    pub fn on_bridge_hello(mut self, callback: BridgeHelloCallback) -> Self {
        self.on_bridge_hello = Some(callback);
        self
    }

    /// Set a callback to be invoked when MCP roots are received or updated.
    #[must_use]
    pub fn on_roots_changed(mut self, callback: RootsChangedCallback) -> Self {
        self.on_roots_changed = Some(callback);
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

            // A fetch still pending from an earlier iteration (armed by a
            // `roots/list_changed` that arrived while a previous fetch was
            // waiting on its response) is served BEFORE this message is
            // dispatched: the message is buffered behind the roots/list
            // response and replayed once the roots are applied.
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
            if request.method != "initialize" {
                self.check_bridge_hello_absent();
            }
            let response = self.handle_request(request)?;
            return Ok(Some(response));
        }

        // Try to parse as notification
        if let Ok(notification) = serde_json::from_str::<Notification>(line) {
            if notification.method != BRIDGE_HELLO_METHOD
                && notification.method != LEGACY_VERSION_MISMATCH_METHOD
            {
                self.check_bridge_hello_absent();
            }
            self.handle_notification(&notification);
            return Ok(None);
        }

        Err(anyhow!(
            "Failed to parse message as request or notification"
        ))
    }

    /// Detects a pre-handshake bridge — one whose hello never arrives (ws41-02).
    ///
    /// A hello-capable bridge injects its [`BRIDGE_HELLO_METHOD`] notification
    /// immediately after forwarding `initialize`, on the same in-order pipe — so
    /// by the time any *other* post-`initialize` message reaches this server,
    /// the hello has either already been dispatched or is never coming. This is
    /// called from [`Self::handle_message`] (the single choke point every
    /// production message crosses, including the `fetch_roots` buffered replay)
    /// for every non-hello message: on the first one after `initialize` with no
    /// hello seen, it reports the bridge as pre-handshake — `None` — exactly
    /// once. A later-arriving hello (should not happen, but cheap to honor)
    /// still fires the callback with its version, and the record-then-clear
    /// machinery corrects the surfaces.
    fn check_bridge_hello_absent(&mut self) {
        if !self.init_seen || self.hello_checked {
            return;
        }
        self.hello_checked = true;
        if let Some(callback) = &self.on_bridge_hello {
            callback(None);
        }
    }

    fn handle_request(&mut self, request: Request) -> Result<Response> {
        debug!("Handling request: {} (id={:?})", request.method, request.id);

        match request.method.as_str() {
            "initialize" => {
                // Anchor for the absent-hello check (ws41-02): a hello-capable
                // bridge's hello follows immediately on the same in-order pipe.
                self.init_seen = true;
                self.handle_initialize(request)
            }
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
                // No roots arm here (misc 169): `initialize` itself arms the
                // fetch, and it always precedes this notification. Arming in
                // both places would issue a second, redundant `roots/list`
                // round on every fresh connection.
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
            BRIDGE_HELLO_METHOD => {
                // The bridge announced its compiled `catenary-mcp` version
                // (ws41-02). Hand it to the daemon-side callback, which compares
                // against the version the daemon links, records/clears the
                // mismatch on the snapshot, and fires the once-per-pairing
                // interrupt. Comparison and surfacing live daemon-side precisely
                // so a pre-handshake bridge (no field) reads as a mismatch.
                // Resolves the absent-hello question; a hello arriving late
                // (after an absent-`None` already fired) still runs the
                // callback, whose record-then-clear machinery corrects the
                // surfaces.
                self.hello_checked = true;
                let bridge_version = notification
                    .params
                    .clone()
                    .and_then(|p| serde_json::from_value::<BridgeHelloParams>(p).ok())
                    .map(|params| params.bridge_version);
                if let Some(callback) = &self.on_bridge_hello {
                    callback(bridge_version.as_deref());
                }
            }
            LEGACY_VERSION_MISMATCH_METHOD => {
                // A ws41-01-generation bridge: it compared versions bridge-side,
                // sent this, and is about to bail. It predates the hello, so it
                // IS a pre-handshake bridge — map it to the same callback as an
                // absent hello. Its `bridgeVersion` param is a git-describe
                // binary string, not a `catenary-mcp` semver; feeding that into
                // the direction comparison could mis-name which side is older,
                // while the pre-handshake label's cure (`/mcp`) is exactly right
                // for a bridge that just tore its own session down.
                self.hello_checked = true;
                if let Some(callback) = &self.on_bridge_hello {
                    callback(None);
                }
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
            // Arm the roots fetch off `initialize` itself (misc 169) — NOT off
            // the `notifications/initialized` that follows it on a fresh client
            // start. A bridge that reattaches after a daemon loss (crash, or a
            // clean `catenary stop` + service restart) replays exactly one
            // captured line, the `initialize` request; `initialized` is a
            // once-per-client-start notification that never re-arrives. Arming
            // here is what makes a replayed init and a fresh init identical:
            // the run loop issues `roots/list` back through the still-live
            // connection as soon as this response is written, so a reattached
            // session re-anchors its roots under the fresh daemon's session key
            // instead of living out its life root-orphaned.
            self.should_fetch_roots = true;
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
    /// the run loop when a fetch is still pending as the next message
    /// arrives, so that message executes against the updated roots.
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
    fn test_initialize_arms_roots_fetch() -> Result<()> {
        // misc 169: the arm rides `initialize` itself, so it is already set
        // before any `notifications/initialized` arrives.
        let mut server = McpServer::new(LoggingServer::new());
        assert!(!server.should_fetch_roots);

        initialize_server(&mut server, true)?;

        assert!(
            server.should_fetch_roots,
            "a roots-capable initialize must arm the roots fetch on its own",
        );
        Ok(())
    }

    #[test]
    fn test_initialized_notification_marks_initialized() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert!(server.initialized);
        Ok(())
    }

    #[test]
    fn test_should_fetch_roots_on_list_changed() -> Result<()> {
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;
        // Clear the init arm (misc 169) so this asserts the list_changed
        // trigger alone, not the one `initialize` already set.
        server.should_fetch_roots = false;

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
        assert!(
            !server.should_fetch_roots,
            "an initialize without the roots capability must not arm the fetch",
        );

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

    use crate::logging::test_support::{
        MessageRecorder, MsgRow, query_all_messages, setup_logging,
    };

    /// Filter to MCP protocol rows only.
    fn mcp_messages(recorder: &Arc<MessageRecorder>) -> Vec<MsgRow> {
        query_all_messages(recorder)
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

    // ── Bridge hello notification tests ────────────────────────────

    #[test]
    fn bridge_hello_delivers_reported_version_to_callback() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let mut server = McpServer::new(LoggingServer::new()).on_bridge_hello(Box::new(move |v| {
            if let Ok(mut w) = seen_clone.lock() {
                w.push(v.map(str::to_string));
            }
        }));

        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: BRIDGE_HELLO_METHOD.to_string(),
            params: Some(serde_json::json!({"bridgeVersion": "0.0.0-fake"})),
        };
        server.handle_notification(&notification);

        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[Some("0.0.0-fake".to_string())],
            "callback should receive the reported bridge version",
        );
    }

    #[test]
    fn bridge_hello_without_version_delivers_none() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let mut server = McpServer::new(LoggingServer::new()).on_bridge_hello(Box::new(move |v| {
            if let Ok(mut w) = seen_clone.lock() {
                w.push(v.map(str::to_string));
            }
        }));

        // A malformed/absent-params hello (a bridge too old to carry the field)
        // still reaches the callback as `None` — the daemon reads that as a
        // mismatch.
        let notification = Notification {
            jsonrpc: "2.0".to_string(),
            method: BRIDGE_HELLO_METHOD.to_string(),
            params: None,
        };
        server.handle_notification(&notification);

        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[None],
            "an absent version reaches the callback as None",
        );
    }

    /// Harness for the absent-hello seam: a server whose `on_bridge_hello`
    /// records every invocation, driven through `handle_message` (the
    /// production choke point), plus the JSON lines a bridge/host produces.
    fn hello_probe() -> (
        McpServer,
        std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    ) {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let server = McpServer::new(LoggingServer::new()).on_bridge_hello(Box::new(move |v| {
            if let Ok(mut w) = seen_clone.lock() {
                w.push(v.map(str::to_string));
            }
        }));
        (server, seen)
    }

    fn initialize_line() -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        })
        .to_string()
    }

    fn hello_line(version: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "method": BRIDGE_HELLO_METHOD,
            "params": {"bridgeVersion": version}
        })
        .to_string()
    }

    const INITIALIZED_LINE: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    const PING_LINE: &str = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;

    #[test]
    fn absent_hello_fires_none_exactly_once() -> Result<()> {
        let (mut server, seen) = hello_probe();

        // A message BEFORE initialize proves nothing (the hello rides after
        // initialize) — no None.
        server.handle_message(INITIALIZED_LINE)?;
        assert!(
            seen.lock().expect("lock").is_empty(),
            "pre-initialize traffic never fires the absent-hello None",
        );

        // initialize, then the host's initialized — with NO hello in between:
        // a pre-handshake bridge, reported as exactly one None.
        server.handle_message(&initialize_line())?;
        server.handle_message(INITIALIZED_LINE)?;
        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[None],
            "an absent hello reads as pre-handshake — one None",
        );

        // Further traffic never re-fires.
        server.handle_message(PING_LINE)?;
        server.handle_message(INITIALIZED_LINE)?;
        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[None],
            "the absent-hello None fires exactly once per connection",
        );
        Ok(())
    }

    #[test]
    fn hello_before_initialized_reports_version_never_none() -> Result<()> {
        let (mut server, seen) = hello_probe();

        // The wire order a hello-capable bridge produces: initialize, hello,
        // then the host's initialized.
        server.handle_message(&initialize_line())?;
        server.handle_message(&hello_line("9.9.9"))?;
        server.handle_message(INITIALIZED_LINE)?;
        server.handle_message(PING_LINE)?;

        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[Some("9.9.9".to_string())],
            "a hello-capable bridge reports its version — no spurious None",
        );
        Ok(())
    }

    #[test]
    fn reconnect_replay_shape_hello_then_request_no_none() -> Result<()> {
        // The reconnect replay (bug 80 + ws41-02): a fresh daemon connection
        // sees the replayed initialize, then the re-sent hello, then ordinary
        // host traffic — `notifications/initialized` never re-arrives. The
        // hello still resolves the question; no None fires on the traffic.
        let (mut server, seen) = hello_probe();

        server.handle_message(&initialize_line())?;
        server.handle_message(&hello_line("9.9.9"))?;
        server.handle_message(PING_LINE)?;

        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[Some("9.9.9".to_string())],
            "the replayed hello resolves the check — no None on later traffic",
        );
        Ok(())
    }

    #[test]
    fn replayed_initialize_arms_roots_without_initialized() -> Result<()> {
        // misc 169: the reattach shape. A bridge that respawned a daemon
        // replays the captured `initialize` and re-sends its hello, then
        // ordinary host traffic — `notifications/initialized` never re-arrives,
        // so it cannot be the roots trigger. The replayed init must arm the
        // fetch itself, and nothing that follows may clear it before
        // `fetch_roots` consumes it; otherwise the reattached session lives out
        // its life with no `mcp:` root contributor (the pinned specimen).
        let mut server = McpServer::new(LoggingServer::new());
        initialize_server(&mut server, true)?;
        assert!(
            server.should_fetch_roots,
            "a replayed initialize must arm the roots fetch with no `initialized` to follow",
        );

        server.handle_message(&hello_line("9.9.9"))?;
        server.handle_message(PING_LINE)?;
        assert!(
            server.should_fetch_roots,
            "the arm stays pending across the replay's trailing traffic",
        );
        Ok(())
    }

    #[test]
    fn legacy_version_mismatch_reads_as_pre_handshake() -> Result<()> {
        // A ws41-01-generation bridge sends `catenary/version-mismatch` (then
        // bails). It maps to the same callback as an absent hello — one None —
        // and resolves the check so nothing double-fires.
        let (mut server, seen) = hello_probe();

        server.handle_message(&initialize_line())?;
        server.handle_message(
            &serde_json::json!({
                "jsonrpc": "2.0", "method": LEGACY_VERSION_MISMATCH_METHOD,
                "params": {"bridgeVersion": "1.2.3-4-gabc"}
            })
            .to_string(),
        )?;
        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[None],
            "the legacy notification reads as a pre-handshake bridge",
        );

        server.handle_message(INITIALIZED_LINE)?;
        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[None],
            "the legacy arm resolves the check — no second None",
        );
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

    /// An `initialize` request as it arrives on the wire, with or without
    /// the client `roots` capability.
    fn initialize_json(with_roots: bool) -> serde_json::Value {
        let capabilities = if with_roots {
            serde_json::json!({"roots": {"listChanged": true}})
        } else {
            serde_json::json!({})
        };
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": capabilities,
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        })
    }

    /// The client's answer to the outbound `roots/list` request `id`.
    fn roots_response_json(id: &str, uri: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"roots": [{"uri": uri}]}
        })
    }

    /// The client-pushed roots-changed notification.
    fn list_changed_json() -> serde_json::Value {
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"})
    }

    /// A server wired only through public builders, plus the roots each
    /// completed `roots/list` round hands to `on_roots_changed`.
    fn roots_probe() -> (McpServer, Arc<std::sync::Mutex<Vec<Root>>>) {
        let seen: Arc<std::sync::Mutex<Vec<Root>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let server =
            McpServer::new(LoggingServer::new()).on_roots_changed(Box::new(move |roots| {
                if let Ok(mut w) = seen_clone.lock() {
                    *w = roots;
                }
                Ok(())
            }));
        (server, seen)
    }

    /// The URIs the last completed `roots/list` round delivered.
    fn seen_uris(seen: &Arc<std::sync::Mutex<Vec<Root>>>) -> Vec<String> {
        seen.lock()
            .expect("roots lock")
            .iter()
            .map(|root| root.uri.clone())
            .collect()
    }

    #[test]
    fn run_fetches_roots_off_a_capable_initialize() -> Result<()> {
        // misc 169: `initialize` is a roots trigger on its own. Driven end to
        // end through `run()` — the public seam — so a trigger that stops
        // being wired to the run loop cannot keep this test green.
        let (mut server, seen) = roots_probe();
        let stdin = synthetic_stdin(&[
            initialize_json(true),
            roots_response_json("catenary-0", "file:///tmp/via_initialize"),
            serde_json::json!({"jsonrpc": "2.0", "id": 42, "method": "ping"}),
        ]);
        let mut writer: Vec<u8> = Vec::new();

        server.run(stdin, &mut writer)?;

        assert_eq!(
            seen_uris(&seen),
            vec!["file:///tmp/via_initialize".to_string()],
            "a roots-capable initialize must complete a roots/list round",
        );
        let output = String::from_utf8(writer)?;
        assert!(
            output.contains("roots/list"),
            "the run loop must put the roots/list request on the wire",
        );
        assert!(
            output.contains(r#""id":42"#),
            "ordinary traffic after the fetch is still answered",
        );
        Ok(())
    }

    #[test]
    fn run_fetches_roots_off_list_changed() -> Result<()> {
        // The second surviving trigger: a client-pushed
        // `notifications/roots/list_changed` — honored even from a client
        // that advertised no roots capability.
        let (mut server, seen) = roots_probe();
        let stdin = synthetic_stdin(&[
            initialize_json(false),
            list_changed_json(),
            roots_response_json("catenary-0", "file:///tmp/via_list_changed"),
        ]);
        let mut writer: Vec<u8> = Vec::new();

        server.run(stdin, &mut writer)?;

        assert_eq!(
            seen_uris(&seen),
            vec!["file:///tmp/via_list_changed".to_string()],
            "list_changed must complete a roots/list round on its own",
        );
        Ok(())
    }

    #[test]
    fn run_issues_no_roots_list_without_a_trigger() -> Result<()> {
        // Neither trigger fires: a capability-less initialize, its
        // `initialized` (which arms nothing — misc 169), and ordinary
        // traffic. Nothing may poll roots from outside those two triggers.
        let (mut server, seen) = roots_probe();
        let stdin = synthetic_stdin(&[
            initialize_json(false),
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            serde_json::json!({"jsonrpc": "2.0", "id": 42, "method": "ping"}),
        ]);
        let mut writer: Vec<u8> = Vec::new();

        server.run(stdin, &mut writer)?;

        assert!(seen_uris(&seen).is_empty(), "no roots round should run");
        let output = String::from_utf8(writer)?;
        assert!(
            !output.contains("roots/list"),
            "no trigger fired — no roots/list belongs on the wire",
        );
        Ok(())
    }

    #[test]
    fn run_buffers_the_next_message_behind_a_pending_fetch() -> Result<()> {
        // A `list_changed` that arrives while a fetch waits on its response
        // re-arms the fetch. The run loop serves that pending fetch before
        // dispatching the next message: the message is buffered behind the
        // roots/list response and replayed against the updated roots.
        let (mut server, seen) = roots_probe();
        let stdin = synthetic_stdin(&[
            initialize_json(true),
            roots_response_json("catenary-0", "file:///tmp/first"),
            list_changed_json(),
            // Interleaved: arrives while the second fetch waits, re-arming it.
            list_changed_json(),
            roots_response_json("catenary-1", "file:///tmp/second"),
            serde_json::json!({"jsonrpc": "2.0", "id": 42, "method": "ping"}),
            roots_response_json("catenary-2", "file:///tmp/third"),
        ]);
        let mut writer: Vec<u8> = Vec::new();

        server.run(stdin, &mut writer)?;

        assert_eq!(
            seen_uris(&seen),
            vec!["file:///tmp/third".to_string()],
            "the re-armed fetch must run against the message that followed it",
        );

        let output = String::from_utf8(writer)?;
        let third_fetch = output
            .find("catenary-2")
            .ok_or_else(|| anyhow!("the pending fetch never reached the wire"))?;
        let ping = output
            .find(r#""id":42"#)
            .ok_or_else(|| anyhow!("ping response not written"))?;
        assert!(
            third_fetch < ping,
            "the ping must be replayed after the roots it was buffered behind",
        );
        Ok(())
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
