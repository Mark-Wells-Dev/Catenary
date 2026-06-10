// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::instance_key::InstanceKey;
use super::params;
use super::server::LspServer;
use super::state::{ServerLifecycle, ServerStatus};
use crate::logging::LoggingServer;

/// Cached diagnostics for a file: `(version, diagnostics)`.
///
/// `version` is the document version from `publishDiagnostics`, if the
/// server includes it.
pub type DiagnosticsCache =
    Arc<std::sync::Mutex<std::collections::HashMap<String, (Option<i32>, Vec<Value>)>>>;

/// Manages communication with an LSP server process.
pub struct LspClient {
    // Server representation (capabilities, state, dispatch, transport)
    server: Arc<LspServer>,

    // Client-local state (not shared with reader)
    encoding: String,
    /// Time when this client was spawned.
    spawn_time: Instant,
    /// Whether the server supports dynamic workspace folder changes
    /// (both `supported` and `change_notifications` are advertised).
    supports_workspace_folders: bool,
    /// Whether the server advertised `textDocumentSync.save` support.
    wants_did_save: bool,
    /// The command used to spawn this server (e.g., "rust-analyzer").
    server_command: String,
    /// Server version from the `initialize` response (`ServerInfo.version`).
    /// Populated after `initialize()` completes; `None` if the server
    /// did not report a version.
    server_version: Option<String>,
    /// Parent message UUID for causation tracking (set before tool dispatch).
    parent_id: Option<String>,
    /// Cancellation token for the current MCP tool call.
    cancel: CancellationToken,
    /// Per-client document state: URI → version.
    ///
    /// Tracks which documents are open on this client and their current
    /// version number. Each client maintains independent versions so
    /// that multi-server dispatch gives each server a clean monotonic
    /// sequence starting at 1.
    open_documents: HashMap<String, i32>,
    /// Workspace folders added via `didChangeWorkspaceFolders` after
    /// initialization. Tracked for deduplication — repeated
    /// `ensure_clients_for_paths` calls don't re-send additions.
    /// The root from `initialize` is NOT included here (it's implicit).
    /// Dropped with the instance on shutdown.
    added_workspace_folders: HashSet<PathBuf>,
}

impl LspClient {
    /// Spawns the LSP server process and starts the response reader task.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server process cannot be spawned.
    /// - Stdin or stdout cannot be captured.
    #[allow(clippy::too_many_arguments, reason = "spawn parameters from ServerDef")]
    pub fn spawn(
        program: &str,
        args: &[&str],
        language_id: &str,
        server_name: &str,
        logging: LoggingServer,
        settings: Option<serde_json::Value>,
        env: Option<&HashMap<String, String>>,
        scope_root: &str,
    ) -> Result<Self> {
        let (client, child_stderr) = Self::spawn_inner(
            program,
            args,
            language_id,
            server_name,
            logging,
            Stdio::piped(),
            settings,
            env,
            scope_root,
        )?;
        if let Some(stderr) = child_stderr {
            Self::spawn_stderr_reader(stderr, server_name);
        }
        Ok(client)
    }

    /// Spawns the LSP server with stderr suppressed (for `catenary doctor`
    /// summary mode).
    ///
    /// # Errors
    ///
    /// Returns an error if the server process cannot be spawned.
    pub fn spawn_quiet(
        program: &str,
        args: &[&str],
        language_id: &str,
        server_name: &str,
        logging: LoggingServer,
        env: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        let (client, _) = Self::spawn_inner(
            program,
            args,
            language_id,
            server_name,
            logging,
            Stdio::null(),
            None,
            env,
            "",
        )?;
        Ok(client)
    }

    /// Spawns the LSP server with stderr piped for capture (for
    /// `catenary doctor <server>` verbose mode).
    ///
    /// Returns the client and the stderr handle for the caller to read.
    ///
    /// # Errors
    ///
    /// Returns an error if the server process cannot be spawned.
    pub fn spawn_for_doctor(
        program: &str,
        args: &[&str],
        language_id: &str,
        server_name: &str,
        logging: LoggingServer,
        env: Option<&HashMap<String, String>>,
    ) -> Result<(Self, Option<tokio::process::ChildStderr>)> {
        Self::spawn_inner(
            program,
            args,
            language_id,
            server_name,
            logging,
            Stdio::piped(),
            None,
            env,
            "",
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "private constructor threading spawn parameters"
    )]
    fn spawn_inner(
        program: &str,
        args: &[&str],
        language_id: &str,
        server_name: &str,
        logging: LoggingServer,
        stderr: Stdio,
        settings: Option<serde_json::Value>,
        env: Option<&HashMap<String, String>>,
        scope_root: &str,
    ) -> Result<(Self, Option<tokio::process::ChildStderr>)> {
        let server = Arc::new(LspServer::new(
            language_id.to_string(),
            server_name.to_string(),
            settings,
        ));

        let (connection, child_stderr) = super::connection::Connection::new(
            program,
            args,
            stderr,
            env,
            &server,
            language_id.to_string(),
            logging,
            server_name,
            scope_root,
        )?;
        server.set_connection(connection);

        Ok((
            Self {
                server,
                encoding: "utf-16".to_string(), // Default per spec
                spawn_time: Instant::now(),
                supports_workspace_folders: false,
                wants_did_save: false,
                server_command: program.to_string(),
                server_version: None,
                parent_id: None,
                cancel: CancellationToken::new(),
                open_documents: HashMap::new(),
                added_workspace_folders: HashSet::new(),
            },
            child_stderr,
        ))
    }

    /// Spawns a background task that reads stderr line-by-line and emits
    /// each line as a `debug!` tracing event with `source = "lsp.stderr"`.
    fn spawn_stderr_reader(stderr: tokio::process::ChildStderr, server_name: &str) {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let server_name = server_name.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = String::from_utf8(std::mem::take(&mut buf))
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                let trimmed = text.trim_end_matches('\n').trim_end_matches('\r');
                if trimmed.is_empty() {
                    continue;
                }
                let line = if trimmed.len() > 4096 {
                    format!("{}… [truncated]", &trimmed[..4096])
                } else {
                    trimmed.to_string()
                };
                debug!(
                    kind = "lsp",
                    method = "stderr",
                    source = crate::source::Source::LspStderr.as_str(),
                    server = %server_name,
                    payload = %line,
                    "{server_name}: {line}",
                );
            }
        });
    }

    /// Sets the parent message ID for causation tracking.
    ///
    /// All subsequent requests and notifications will carry this parent ID
    /// until it is changed or cleared.
    pub fn set_parent_id(&mut self, parent_id: Option<String>) {
        self.parent_id = parent_id;
    }

    /// Sets the cancellation token for the current MCP tool call.
    ///
    /// The token is checked during [`Connection::request`] — if triggered,
    /// the LSP request is aborted with `$/cancelRequest`.
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel = token;
    }

    /// Returns an error if the server does not support the given capability.
    fn require_capability(&self, method: &str, check: fn(&LspServer) -> bool) -> Result<()> {
        if !check(&self.server) {
            return Err(anyhow!("server does not support {method}"));
        }
        Ok(())
    }

    /// Sends a request and waits for the response.
    ///
    /// Delegates to [`LspServer::request`] for transport and failure
    /// detection, returning the raw JSON response. On success, transitions
    /// `Probing` → `Healthy` (any successful response proves the server works).
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let result = self
            .server
            .request(method, params, self.parent_id.as_deref(), &self.cancel)
            .await?;
        self.server.try_transition_probing_to_healthy();
        Ok(result)
    }

    /// Sends a notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.server
            .notify(method, params, self.parent_id.as_deref())
            .await
    }

    /// Runs the health probe: sends `documentSymbol` to verify the server
    /// can respond. Transitions `Probing` → `Healthy` on success, `Probing` →
    /// `Failed` on error.
    ///
    /// Uses the same file the diagnostics pipeline is processing — the
    /// `didOpen` that the pipeline sends serves as the probe's `didOpen`.
    ///
    /// No wall-clock timeout wraps the request: under heavy CPU load a
    /// starved-but-alive server can take well over a minute to answer, and a
    /// wall clock would falsely mark it `Failed` (it fails in production, not
    /// just flakes). The bound is catenary-proc instead — [`Connection::request`]
    /// detects server death (`ProcessMonitor`) and a genuinely stuck server
    /// (CPU-tick budget), and the connection's cancellation token tears the
    /// request down if the diagnostics client disconnects.
    ///
    /// Returns `true` if the server is now `Healthy`.
    pub async fn run_health_probe(&self, uri: &str) -> bool {
        if self.server.lifecycle() != ServerLifecycle::Probing {
            return !self.server.lifecycle().is_terminal();
        }

        debug!("Running health probe on {uri}");

        match self
            .request("textDocument/documentSymbol", params::document_symbols(uri))
            .await
        {
            Ok(_) => {
                // request() already called try_transition_probing_to_healthy
                debug!("Health probe succeeded — server is Healthy");
                true
            }
            Err(e) => {
                debug!("Health probe failed: {e}");
                self.server.set_lifecycle(ServerLifecycle::Failed);
                false
            }
        }
    }

    /// Performs the LSP initialize handshake.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A root path is invalid.
    /// - The initialize request fails.
    /// - The server fails to respond.
    pub async fn initialize(
        &mut self,
        roots: &[PathBuf],
        initialization_options: Option<serde_json::Value>,
    ) -> Result<Value> {
        let workspace_folders: Vec<(String, String)> = roots
            .iter()
            .map(|root| {
                let uri = format!("file://{}", root.display());
                let name = root.file_name().map_or_else(
                    || "workspace".to_string(),
                    |s| s.to_string_lossy().to_string(),
                );
                (uri, name)
            })
            .collect();

        let folder_refs: Vec<(&str, &str)> = workspace_folders
            .iter()
            .map(|(uri, name)| (uri.as_str(), name.as_str()))
            .collect();

        let init_params = params::initialize(
            std::process::id(),
            &folder_refs,
            initialization_options.as_ref(),
        );

        let raw = self.request("initialize", init_params).await?;

        let caps = raw
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::default()));

        // Extract negotiated encoding
        if let Some(enc) = super::extract::position_encoding(&caps) {
            self.encoding = enc.to_string();
            debug!("Negotiated position encoding: {}", self.encoding);
        } else {
            debug!("Server did not specify position encoding, defaulting to UTF-16");
            self.encoding = "utf-16".to_string();
        }

        // Extract workspace folders capability
        self.supports_workspace_folders = super::extract::supports_workspace_folders(&caps);
        debug!(
            "Server workspace folders support: {}",
            self.supports_workspace_folders
        );

        // Extract textDocumentSync.save capability
        self.wants_did_save = super::extract::wants_did_save(&caps);
        debug!(
            "[{}] server wants didSave: {}",
            self.server.language_id(),
            self.wants_did_save
        );

        // Store server info and set capabilities on existing server profile
        self.server_version = super::extract::server_version(&raw).map(str::to_string);
        self.server.set_capabilities(caps);

        // Send initialized notification
        self.notify("initialized", json!({})).await?;

        // Push current settings. Pull-model servers will also send
        // workspace/configuration requests, but the push is harmless
        // and required by legacy servers that don't use the pull model.
        let settings = self
            .server
            .settings()
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        self.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": settings}),
        )
        .await?;

        // Mark as probing — server unproven until health probe or
        // first successful tool request transitions to Healthy.
        self.server.set_lifecycle(ServerLifecycle::Probing);

        Ok(raw)
    }

    /// Returns the negotiated position encoding.
    #[must_use]
    pub fn encoding(&self) -> &str {
        &self.encoding
    }

    /// Returns the server capabilities from the `initialize` response.
    ///
    /// Returns an empty object before `initialize()` completes.
    #[must_use]
    pub fn capabilities(&self) -> &Value {
        self.server.capabilities()
    }

    /// Sends shutdown request and exit notification.
    ///
    /// # Errors
    ///
    /// Returns an error if the shutdown request or exit notification fails.
    pub async fn shutdown(&mut self) -> Result<()> {
        // shutdown response varies by server (null, true, etc.) - ignore result
        let _: serde_json::Value = self.request("shutdown", serde_json::Value::Null).await?;
        self.notify("exit", serde_json::Value::Null).await?;
        Ok(())
    }

    /// Notifies the LSP server that a document was opened.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_open(
        &self,
        uri: &str,
        language_id: &str,
        version: i32,
        text: &str,
    ) -> Result<()> {
        self.notify(
            "textDocument/didOpen",
            params::did_open(uri, language_id, version, text),
        )
        .await
    }

    /// Notifies the LSP server that a document changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change(&self, uri: &str, version: i32, text: &str) -> Result<()> {
        self.notify(
            "textDocument/didChange",
            params::did_change(uri, version, text),
        )
        .await
    }

    /// Notifies the LSP server that a document was saved.
    ///
    /// This triggers flycheck (e.g., `cargo check`) on servers that only
    /// run diagnostics on save, like rust-analyzer.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_save(&self, uri: &str) -> Result<()> {
        self.notify("textDocument/didSave", params::did_save(uri))
            .await
    }

    /// Notifies the LSP server that a document was closed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_close(&self, uri: &str) -> Result<()> {
        self.notify("textDocument/didClose", params::did_close(uri))
            .await
    }

    /// Notifies the server about filesystem changes for watched files.
    ///
    /// `changes` is a slice of `(uri, FileChangeType as u8)` pairs.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change_watched_files(&self, changes: &[(&str, u8)]) -> Result<()> {
        self.notify(
            "workspace/didChangeWatchedFiles",
            params::did_change_watched_files(changes),
        )
        .await
    }

    /// Adds a workspace folder to this server instance.
    ///
    /// Sends `workspace/didChangeWorkspaceFolders` and tracks the folder
    /// for deduplication. Returns `false` without sending if the folder
    /// was already added or the server doesn't support workspace folders.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn add_workspace_folder(&mut self, folder: &Path) -> Result<bool> {
        if !self.supports_workspace_folders
            || !self.added_workspace_folders.insert(folder.to_path_buf())
        {
            return Ok(false);
        }
        let uri = format!("file://{}", folder.display());
        let name = folder.file_name().map_or_else(
            || "workspace".to_string(),
            |s| s.to_string_lossy().to_string(),
        );
        let added = [(uri.as_str(), name.as_str())];
        self.notify(
            "workspace/didChangeWorkspaceFolders",
            params::did_change_workspace_folders(&added, &[]),
        )
        .await?;
        Ok(true)
    }

    /// Sends `workspace/didChangeConfiguration` notification.
    ///
    /// Payload is `{ settings: {} }` — servers that care will pull
    /// updated config via `workspace/configuration` requests, which
    /// are now answered with `scopeUri`-resolved settings (from 1d-01).
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change_configuration(&self) -> Result<()> {
        self.notify(
            "workspace/didChangeConfiguration",
            params::did_change_configuration(),
        )
        .await
    }

    /// Tests whether a position is a renameable symbol.
    ///
    /// Returns a non-null `Value` for symbols, `Value::Null` for keywords
    /// and non-symbol positions. Used as a cheap discriminator before full
    /// enrichment in the rg-bootstrap path.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_rename(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.require_capability("textDocument/prepareRename", LspServer::supports_rename)?;
        self.request(
            "textDocument/prepareRename",
            params::prepare_rename(uri, line, character),
        )
        .await
    }

    /// Gets the definition location for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn definition(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.require_capability("textDocument/definition", LspServer::supports_definition)?;
        self.request(
            "textDocument/definition",
            params::definition(uri, line, character),
        )
        .await
    }

    /// Gets the type definition location for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn type_definition(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.require_capability(
            "textDocument/typeDefinition",
            LspServer::supports_type_definition,
        )?;
        self.request(
            "textDocument/typeDefinition",
            params::type_definition(uri, line, character),
        )
        .await
    }

    /// Gets implementation locations for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn implementation(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.require_capability(
            "textDocument/implementation",
            LspServer::supports_implementation,
        )?;
        self.request(
            "textDocument/implementation",
            params::implementation(uri, line, character),
        )
        .await
    }

    /// Gets all references to a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Value> {
        self.require_capability("textDocument/references", LspServer::supports_references)?;
        self.request(
            "textDocument/references",
            params::references(uri, line, character, include_declaration),
        )
        .await
    }

    /// Gets document symbols (outline) for a file.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn document_symbols(&self, uri: &str) -> Result<Value> {
        self.require_capability(
            "textDocument/documentSymbol",
            LspServer::supports_document_symbols,
        )?;
        self.request("textDocument/documentSymbol", params::document_symbols(uri))
            .await
    }

    /// Searches for symbols across the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Value> {
        self.require_capability("workspace/symbol", LspServer::supports_workspace_symbols)?;
        self.request("workspace/symbol", params::workspace_symbols(query))
            .await
    }

    /// Resolves additional properties (e.g. `location.range`) for a workspace symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn workspace_symbol_resolve(&self, symbol: &Value) -> Result<Value> {
        self.request("workspaceSymbol/resolve", symbol.clone())
            .await
    }

    /// Returns whether the server advertises `workspaceSymbolProvider.resolveProvider`.
    #[must_use]
    pub fn supports_workspace_symbol_resolve(&self) -> bool {
        self.server.supports_workspace_symbol_resolve()
    }

    /// Returns whether the server advertises `diagnosticProvider` (pull model).
    #[must_use]
    pub fn supports_pull_diagnostics(&self) -> bool {
        self.server.supports_pull_diagnostics()
    }

    /// Returns whether the server advertises `renameProvider`.
    #[must_use]
    pub fn supports_rename(&self) -> bool {
        self.server.supports_rename()
    }

    /// Returns whether the server advertises `typeHierarchyProvider`.
    #[must_use]
    pub fn supports_type_hierarchy(&self) -> bool {
        self.server.supports_type_hierarchy()
    }

    /// Returns whether the server advertises `codeActionProvider`.
    #[must_use]
    pub fn supports_code_action(&self) -> bool {
        self.server.supports_code_action()
    }

    /// Prepares call hierarchy for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_call_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Value> {
        self.require_capability(
            "textDocument/prepareCallHierarchy",
            LspServer::supports_call_hierarchy,
        )?;
        self.request(
            "textDocument/prepareCallHierarchy",
            params::prepare_call_hierarchy(uri, line, character),
        )
        .await
    }

    /// Gets incoming calls to a call hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn incoming_calls(&self, item: &Value) -> Result<Value> {
        self.request("callHierarchy/incomingCalls", params::incoming_calls(item))
            .await
    }

    /// Gets outgoing calls from a call hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn outgoing_calls(&self, item: &Value) -> Result<Value> {
        self.request("callHierarchy/outgoingCalls", params::outgoing_calls(item))
            .await
    }

    /// Prepares type hierarchy for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_type_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Value> {
        self.require_capability(
            "textDocument/prepareTypeHierarchy",
            LspServer::supports_type_hierarchy,
        )?;
        self.request(
            "textDocument/prepareTypeHierarchy",
            params::prepare_type_hierarchy(uri, line, character),
        )
        .await
    }

    /// Gets supertypes of a type hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn supertypes(&self, item: &Value) -> Result<Value> {
        self.request("typeHierarchy/supertypes", params::supertypes(item))
            .await
    }

    /// Gets subtypes of a type hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn subtypes(&self, item: &Value) -> Result<Value> {
        self.request("typeHierarchy/subtypes", params::subtypes(item))
            .await
    }

    /// Gets code actions (quick fixes) for a range.
    ///
    /// Bakes in `only: ["quickfix"]` because the only caller (notify.rs)
    /// always wants quickfixes.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn code_action(
        &self,
        uri: &str,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
        diagnostics: &[Value],
    ) -> Result<Value> {
        self.require_capability("textDocument/codeAction", LspServer::supports_code_action)?;
        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_line, "character": start_char },
                "end": { "line": end_line, "character": end_char }
            },
            "context": {
                "diagnostics": diagnostics,
                "only": ["quickfix"]
            }
        });
        self.request("textDocument/codeAction", params).await
    }

    /// Pulls diagnostics from the server via `textDocument/diagnostic`.
    ///
    /// Returns the diagnostics array from the response, or an empty
    /// vec on error/timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn pull_diagnostics(&self, uri: &str) -> Result<Vec<Value>> {
        self.require_capability(
            "textDocument/diagnostic",
            LspServer::supports_pull_diagnostics,
        )?;
        let result = self
            .request(
                "textDocument/diagnostic",
                params::text_document_diagnostic(uri),
            )
            .await?;
        Ok(super::extract::document_diagnostic_report(&result))
    }

    /// Gets cached diagnostics for a specific URI.
    pub fn get_diagnostics(&self, uri: &str) -> Vec<Value> {
        let cache = self
            .server
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .get(uri)
            .map(|(_, diags)| diags.clone())
            .unwrap_or_default()
    }

    /// Removes cached diagnostics for the given URIs.
    ///
    /// Clears stale entries that may have been populated by
    /// file-watcher notifications before the diagnostic batch
    /// opens files. Only fresh `publishDiagnostics` from the
    /// batch settle phase should survive to retrieval.
    pub fn clear_diagnostics_for(&self, uris: &[&str]) {
        let mut cache = self
            .server
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for uri in uris {
            cache.remove(*uri);
        }
    }

    /// Returns whether the server advertised `textDocumentSync.save` support.
    ///
    /// When `false`, `did_save` should not be sent — the server doesn't
    /// want it and may not run diagnostics on save.
    #[must_use]
    pub const fn wants_did_save(&self) -> bool {
        self.wants_did_save
    }

    /// Returns the PID of the server process, if available.
    #[allow(dead_code, reason = "Used by diagnostics tests and session status")]
    pub(crate) fn pid(&self) -> Option<u32> {
        self.server.pid()
    }

    /// Returns the underlying server representation.
    ///
    /// Used by the idle detection loop and diagnostics pipeline, which
    /// operate directly on `Arc<LspServer>`.
    #[must_use]
    pub const fn server(&self) -> &Arc<LspServer> {
        &self.server
    }

    /// Returns the command used to spawn this server (e.g., "rust-analyzer").
    #[must_use]
    pub fn server_command(&self) -> &str {
        &self.server_command
    }

    /// Returns the server version from the LSP `initialize` response.
    #[must_use]
    pub fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }

    /// Returns the language identifier for this client (e.g., "rust", "python").
    #[must_use]
    pub fn language(&self) -> &str {
        self.server.language_id()
    }

    /// Returns the server config name (e.g., "rust-analyzer", "pyright").
    #[must_use]
    pub fn server_name(&self) -> &str {
        self.server.server_name()
    }

    /// Returns whether this client has a document open by URI.
    #[must_use]
    pub fn is_document_open(&self, uri: &str) -> bool {
        self.open_documents.contains_key(uri)
    }

    /// Registers an open and returns `(first_open, version)`.
    ///
    /// First open returns `(true, 1)` — caller sends `didOpen`.
    /// Subsequent opens increment the version and return `(false, version)`
    /// — caller sends `didChange`.
    pub fn open_document(&mut self, uri: &str) -> (bool, i32) {
        use std::collections::hash_map::Entry;
        match self.open_documents.entry(uri.to_string()) {
            Entry::Occupied(mut e) => {
                *e.get_mut() += 1;
                (false, *e.get())
            }
            Entry::Vacant(e) => {
                e.insert(1);
                (true, 1)
            }
        }
    }

    /// Closes a document while the caller holds the lock.
    ///
    /// Removes the URI from per-client tracking and sends `didClose`.
    /// Eliminates the lock gap that would exist if the caller dropped
    /// the guard and called a separate close method.
    pub async fn close_tracked_document(&mut self, uri: &str) {
        if self.open_documents.remove(uri).is_some() {
            let _ = self.did_close(uri).await;
        }
    }

    /// Returns whether the server supports dynamic workspace folder changes.
    #[must_use]
    pub const fn supports_workspace_folders(&self) -> bool {
        self.supports_workspace_folders
    }

    /// Returns whether the LSP server process is still running.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.server.is_alive()
    }

    /// Returns the current server lifecycle state.
    #[must_use]
    pub fn lifecycle(&self) -> ServerLifecycle {
        self.server.lifecycle()
    }

    /// Returns time since server spawned.
    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.spawn_time.elapsed()
    }

    /// Returns detailed status for this server.
    pub fn status(&self, key: &InstanceKey) -> ServerStatus {
        let (title, message, percentage) = {
            let progress = self
                .server
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let primary = progress.primary_progress();
            let title = primary.map(|p| p.title.clone());
            let message = primary.and_then(|p| p.message.clone());
            let percentage = primary.and_then(|p| p.percentage);
            drop(progress);
            (title, message, percentage)
        };

        ServerStatus {
            language: key.language_id.clone(),
            server_name: key.server.clone(),
            scope_kind: key.scope.kind_str().to_string(),
            scope_root: key
                .scope
                .root_path()
                .map_or_else(String::new, |p| p.display().to_string()),
            state: self.lifecycle(),
            progress_title: title,
            progress_message: message,
            progress_percentage: percentage,
            uptime_secs: self.uptime().as_secs(),
        }
    }

    /// Waits until server is ready to accept requests.
    ///
    /// Returns `true` for `Healthy` and `Probing` — both states accept
    /// requests. `Probing` allows tool requests to be self-testing: a
    /// successful response transitions `Probing` → `Healthy` via
    /// [`LspServer::try_transition_probing_to_healthy`].
    ///
    /// Watches the lifecycle enum — wakes on every lifecycle transition.
    /// No budget, no tick counting, no process sampling. Servers that
    /// pass health are waited for patiently. `Connection::request`
    /// catches individual stuck requests with its own failure detection.
    ///
    /// Returns `true` if ready, `false` if server failed or died.
    pub async fn wait_ready(&self) -> bool {
        loop {
            let lifecycle = self.server.lifecycle();
            match lifecycle {
                ServerLifecycle::Healthy | ServerLifecycle::Probing => return true,
                ServerLifecycle::Failed | ServerLifecycle::Dead => return false,
                _ => {} // Initializing, Busy — keep waiting
            }
            self.server.state_notify.notified().await;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::logging::test_support::{MessageRecorder, setup_logging};
    use crate::lsp::state::ServerLifecycle;
    use crate::lsp::test_support::mockls_bin;
    use std::sync::Arc;

    const MOCK_LANG: &str = "tCl1x";

    fn test_logging() -> LoggingServer {
        LoggingServer::new()
    }

    /// Poll the recorder until a stderr message appears, returning its payload.
    async fn poll_stderr_payload(recorder: &Arc<MessageRecorder>) -> Option<String> {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(row) = recorder
                .rows()
                .into_iter()
                .find(|m| m.r#type == "lsp" && m.method == "stderr")
            {
                return Some(row.payload);
            }
        }
        None
    }

    /// Spawn mockls and initialize with defaults.
    async fn spawn_and_init(extra_args: &[&str]) -> Result<(LspClient, tempfile::TempDir)> {
        let dir = tempfile::tempdir()?;
        let bin = mockls_bin();
        let bin_str = bin.to_str().expect("mockls path is UTF-8");
        let mut args = vec![MOCK_LANG];
        args.extend_from_slice(extra_args);
        let mut client = LspClient::spawn(
            bin_str,
            &args,
            MOCK_LANG,
            MOCK_LANG,
            test_logging(),
            None,
            None,
            "",
        )?;
        client.initialize(&[dir.path().to_path_buf()], None).await?;
        Ok((client, dir))
    }

    // ── Accessor tests after initialize ─────────────────────────────

    #[tokio::test]
    async fn accessors_after_initialize() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;

        // encoding defaults to utf-16
        assert_eq!(client.encoding(), "utf-16");

        // language and server_name reflect spawn arguments
        assert_eq!(client.language(), MOCK_LANG);
        assert_eq!(client.server_name(), MOCK_LANG);

        // server_command is the mockls binary path
        assert!(
            client.server_command().contains("mockls"),
            "server_command should contain mockls, got: {}",
            client.server_command()
        );

        // capabilities should have hoverProvider (mockls default)
        assert!(
            client.capabilities().get("hoverProvider").is_some(),
            "capabilities should include hoverProvider"
        );

        // pid should be Some (server is running)
        assert!(client.pid().is_some(), "server pid should be available");

        // server_version is None when mockls doesn't report serverInfo
        assert_eq!(client.server_version(), None);

        // wants_did_save is false without --advertise-save
        assert!(!client.wants_did_save());

        // lifecycle should be Probing after initialize
        assert_eq!(client.lifecycle(), ServerLifecycle::Probing);

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn accessors_with_server_info() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let bin = mockls_bin();
        let bin_str = bin.to_str().expect("mockls path is UTF-8");

        let mut env = HashMap::new();
        env.insert("CATENARY_TEST_VER".to_string(), "1.2.3".to_string());

        let mut client = LspClient::spawn(
            bin_str,
            &[
                MOCK_LANG,
                "--report-env",
                "CATENARY_TEST_VER",
                "--advertise-save",
            ],
            MOCK_LANG,
            MOCK_LANG,
            test_logging(),
            None,
            Some(&env),
            "",
        )?;
        client.initialize(&[dir.path().to_path_buf()], None).await?;

        assert_eq!(client.server_version(), Some("1.2.3"));
        assert!(client.wants_did_save());

        client.shutdown().await?;
        Ok(())
    }

    // ── Capability query tests ──────────────────────────────────────

    #[tokio::test]
    async fn supports_capabilities_default_mockls() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;

        // mockls advertises these by default
        assert!(client.supports_rename());
        assert!(client.supports_type_hierarchy());
        assert!(client.supports_code_action());

        // mockls does NOT advertise these without flags
        assert!(!client.supports_pull_diagnostics());
        assert!(!client.supports_workspace_symbol_resolve());

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn supports_capabilities_with_flags() -> Result<()> {
        let (mut client, _dir) =
            spawn_and_init(&["--pull-diagnostics", "--resolve-provider"]).await?;

        assert!(client.supports_pull_diagnostics());
        assert!(client.supports_workspace_symbol_resolve());

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn supports_capabilities_disabled_by_flags() -> Result<()> {
        let (mut client, _dir) =
            spawn_and_init(&["--no-rename", "--no-type-hierarchy", "--no-code-actions"]).await?;

        assert!(!client.supports_rename());
        assert!(!client.supports_type_hierarchy());
        assert!(!client.supports_code_action());

        client.shutdown().await?;
        Ok(())
    }

    // ── require_capability tests ────────────────────────────────────

    #[tokio::test]
    async fn require_capability_passes_when_supported() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;

        // rename is supported by default in mockls
        assert!(
            client
                .require_capability("textDocument/rename", LspServer::supports_rename)
                .is_ok()
        );

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn require_capability_errors_when_unsupported() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;

        // pull diagnostics not supported without --pull-diagnostics
        let err = client
            .require_capability(
                "textDocument/diagnostic",
                LspServer::supports_pull_diagnostics,
            )
            .expect_err("should error when capability is unsupported");
        assert!(
            err.to_string().contains("does not support"),
            "error should mention missing support: {err}"
        );

        client.shutdown().await?;
        Ok(())
    }

    // ── open_document version tracking ──────────────────────────────

    #[tokio::test]
    async fn open_document_version_tracking() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;

        // First open → (true, 1)
        let (first, version) = client.open_document("file:///a.rs");
        assert!(first, "first open should return true");
        assert_eq!(version, 1);

        // Same URI again → (false, 2)
        let (first, version) = client.open_document("file:///a.rs");
        assert!(!first, "second open should return false");
        assert_eq!(version, 2);

        // Third open → (false, 3)
        let (first, version) = client.open_document("file:///a.rs");
        assert!(!first, "third open should return false");
        assert_eq!(version, 3);

        // Different URI → (true, 1)
        let (first, version) = client.open_document("file:///b.rs");
        assert!(first, "first open of different URI should return true");
        assert_eq!(version, 1);

        client.shutdown().await?;
        Ok(())
    }

    // ── run_health_probe tests ──────────────────────────────────────

    #[tokio::test]
    async fn health_probe_transitions_probing_to_healthy() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("probe.{MOCK_LANG}"));
        std::fs::write(&script, "fn hello\nhello\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;
        assert_eq!(client.lifecycle(), ServerLifecycle::Probing);

        let uri = format!("file://{}", script.display());
        client
            .did_open(&uri, MOCK_LANG, 1, "fn hello\nhello\n")
            .await?;

        let result = client.run_health_probe(&uri).await;
        assert!(result, "health probe should return true");
        assert_eq!(client.lifecycle(), ServerLifecycle::Healthy);

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn health_probe_skips_when_already_healthy() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("probe.{MOCK_LANG}"));
        std::fs::write(&script, "fn hello\nhello\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client
            .did_open(&uri, MOCK_LANG, 1, "fn hello\nhello\n")
            .await?;

        // First probe → Healthy
        assert!(client.run_health_probe(&uri).await);
        assert_eq!(client.lifecycle(), ServerLifecycle::Healthy);

        // Second probe on Healthy → returns true, no state change
        assert!(client.run_health_probe(&uri).await);
        assert_eq!(client.lifecycle(), ServerLifecycle::Healthy);

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn health_probe_returns_false_on_dead() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;

        // Force lifecycle to Dead
        client.server.set_lifecycle(ServerLifecycle::Dead);

        let result = client.run_health_probe("file:///fake.rs").await;
        assert!(!result, "health probe should return false when Dead");

        client.shutdown().await?;
        Ok(())
    }

    // ── wait_ready tests ────────────────────────────────────────────

    #[tokio::test]
    async fn wait_ready_returns_true_when_probing() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;
        assert_eq!(client.lifecycle(), ServerLifecycle::Probing);

        let ready = client.wait_ready().await;
        assert!(ready, "wait_ready should return true for Probing");

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn wait_ready_returns_false_on_failed() -> Result<()> {
        let (mut client, _dir) = spawn_and_init(&[]).await?;

        client.server.set_lifecycle(ServerLifecycle::Failed);

        let ready = client.wait_ready().await;
        assert!(!ready, "wait_ready should return false for Failed");

        client.shutdown().await?;
        Ok(())
    }

    // ── set_cancel_token test ───────────────────────────────────────

    #[tokio::test]
    async fn set_cancel_token_causes_request_cancellation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("cancel.{MOCK_LANG}"));
        std::fs::write(&script, "fn hello\nhello\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client
            .did_open(&uri, MOCK_LANG, 1, "fn hello\nhello\n")
            .await?;

        // Set a pre-cancelled token
        let token = CancellationToken::new();
        token.cancel();
        client.set_cancel_token(token);

        // Request should fail because the token is already cancelled
        let result = client.definition(&uri, 1, 0).await;
        assert!(result.is_err(), "request with cancelled token should fail");

        // Reset to a fresh token so shutdown works
        client.set_cancel_token(CancellationToken::new());
        client.shutdown().await?;
        Ok(())
    }

    // ── spawn_stderr_reader tests ───────────────────────────────────

    #[tokio::test]
    async fn stderr_capture_recorded_in_db() -> Result<()> {
        let (logging, db, _guard) = setup_logging();

        let dir = tempfile::tempdir()?;
        let bin = mockls_bin();
        let bin_str = bin.to_str().expect("mockls path is UTF-8");

        let mut client = LspClient::spawn(
            bin_str,
            &[MOCK_LANG, "--stderr-message", "unit_stderr_test_line"],
            MOCK_LANG,
            MOCK_LANG,
            logging,
            None,
            None,
            "",
        )?;
        client.initialize(&[dir.path().to_path_buf()], None).await?;

        let payload = poll_stderr_payload(&db)
            .await
            .expect("stderr event should appear in DB");
        assert!(
            payload.contains("unit_stderr_test_line"),
            "payload should contain the stderr text, got: {payload}"
        );
        assert!(
            !payload.contains("truncated"),
            "short line should not be truncated"
        );

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn stderr_line_at_boundary_not_truncated() -> Result<()> {
        let (logging, db, _guard) = setup_logging();

        let dir = tempfile::tempdir()?;
        let bin = mockls_bin();
        let bin_str = bin.to_str().expect("mockls path is UTF-8");

        let mut client = LspClient::spawn(
            bin_str,
            &[MOCK_LANG, "--stderr-length", "4096"],
            MOCK_LANG,
            MOCK_LANG,
            logging,
            None,
            None,
            "",
        )?;
        client.initialize(&[dir.path().to_path_buf()], None).await?;

        let payload = poll_stderr_payload(&db)
            .await
            .expect("stderr event should appear in DB");
        assert!(
            !payload.contains("truncated"),
            "4096-char line should NOT be truncated"
        );

        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn stderr_line_above_boundary_truncated() -> Result<()> {
        let (logging, db, _guard) = setup_logging();

        let dir = tempfile::tempdir()?;
        let bin = mockls_bin();
        let bin_str = bin.to_str().expect("mockls path is UTF-8");

        let mut client = LspClient::spawn(
            bin_str,
            &[MOCK_LANG, "--stderr-length", "5000"],
            MOCK_LANG,
            MOCK_LANG,
            logging,
            None,
            None,
            "",
        )?;
        client.initialize(&[dir.path().to_path_buf()], None).await?;

        let payload = poll_stderr_payload(&db)
            .await
            .expect("stderr event should appear in DB");
        assert!(
            payload.contains("truncated"),
            "5000-char line should be truncated, got len: {}",
            payload.len()
        );

        client.shutdown().await?;
        Ok(())
    }

    // ── Document operations + request tests ────────────────────────

    /// Verifies that `did_open` delivers the document to the server.
    ///
    /// If `did_open` were a no-op, the server would not know about the
    /// file and `definition` would return null.
    #[tokio::test]
    async fn did_open_delivers_document_to_server() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("open.{MOCK_LANG}"));
        std::fs::write(&script, "fn hello\nhello\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client
            .did_open(&uri, MOCK_LANG, 1, "fn hello\nhello\n")
            .await?;

        // definition depends on the document being open in mockls
        let result = client.definition(&uri, 1, 0).await?;
        assert!(
            result.get("uri").is_some(),
            "definition should return a location with uri, got: {result}"
        );

        client.shutdown().await?;
        Ok(())
    }

    /// Verifies that `did_save` and `did_close` reach the server.
    ///
    /// Uses mockls `--notification-log` to confirm the notifications
    /// were delivered, not silently dropped.
    #[tokio::test]
    async fn did_save_and_close_reach_server() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log_path = dir.path().join("notif.jsonl");
        let log_str = log_path.to_str().expect("path is UTF-8");
        let script = dir.path().join(format!("save.{MOCK_LANG}"));
        std::fs::write(&script, "fn hello\n")?;

        let (mut client, _dir) =
            spawn_and_init(&["--advertise-save", "--notification-log", log_str]).await?;

        let uri = format!("file://{}", script.display());
        client.did_open(&uri, MOCK_LANG, 1, "fn hello\n").await?;
        client.did_save(&uri).await?;
        client.did_close(&uri).await?;
        client.shutdown().await?;

        let log = std::fs::read_to_string(&log_path)?;
        let methods: Vec<String> = log
            .lines()
            .filter_map(|line| {
                serde_json::from_str::<Value>(line)
                    .ok()
                    .and_then(|v| v.get("method")?.as_str().map(String::from))
            })
            .collect();

        assert!(
            methods.contains(&"textDocument/didOpen".to_string()),
            "notification log should contain didOpen: {methods:?}"
        );
        assert!(
            methods.contains(&"textDocument/didSave".to_string()),
            "notification log should contain didSave: {methods:?}"
        );
        assert!(
            methods.contains(&"textDocument/didClose".to_string()),
            "notification log should contain didClose: {methods:?}"
        );

        Ok(())
    }

    /// Verifies that `definition` returns a real location, not a default.
    #[tokio::test]
    async fn definition_returns_location() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("def.{MOCK_LANG}"));
        std::fs::write(&script, "fn hello\nhello\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client
            .did_open(&uri, MOCK_LANG, 1, "fn hello\nhello\n")
            .await?;

        let result = client.definition(&uri, 1, 0).await?;
        assert_eq!(
            result.get("uri").and_then(Value::as_str),
            Some(uri.as_str()),
            "definition should point back to the same file"
        );
        assert!(
            result.get("range").is_some(),
            "definition should include a range"
        );

        client.shutdown().await?;
        Ok(())
    }

    /// Verifies that `prepare_rename` returns a range for renameable symbols.
    #[tokio::test]
    async fn prepare_rename_returns_range() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("rename.{MOCK_LANG}"));
        std::fs::write(&script, "fn hello\nhello\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client
            .did_open(&uri, MOCK_LANG, 1, "fn hello\nhello\n")
            .await?;

        // "hello" on line 1 is a renameable identifier (not a keyword)
        let result = client.prepare_rename(&uri, 1, 0).await?;
        assert!(
            result.get("range").is_some(),
            "prepare_rename should return a range for identifiers, got: {result}"
        );

        client.shutdown().await?;
        Ok(())
    }

    /// Verifies that `workspace_symbols` returns matching symbols.
    #[tokio::test]
    async fn workspace_symbols_returns_results() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("sym.{MOCK_LANG}"));
        std::fs::write(&script, "fn my_func\nconst MY_CONST\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client
            .did_open(&uri, MOCK_LANG, 1, "fn my_func\nconst MY_CONST\n")
            .await?;

        let result = client.workspace_symbols("my_func").await?;
        let symbols = result.as_array().expect("workspace_symbols returns array");
        assert!(!symbols.is_empty(), "workspace_symbols should find my_func");
        assert!(
            symbols
                .iter()
                .any(|s| s.get("name").and_then(Value::as_str) == Some("my_func")),
            "workspace_symbols should contain my_func: {result}"
        );

        client.shutdown().await?;
        Ok(())
    }

    /// Verifies that `outgoing_calls` returns call targets.
    #[tokio::test]
    async fn outgoing_calls_returns_targets() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("calls.{MOCK_LANG}"));
        let content = "fn caller\n  callee\nfn callee\n";
        std::fs::write(&script, content)?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client.did_open(&uri, MOCK_LANG, 1, content).await?;

        // Prepare call hierarchy for "caller" on line 0
        let prep = client.prepare_call_hierarchy(&uri, 0, 3).await?;
        let items = prep.as_array().expect("prepare returns array");
        assert!(
            !items.is_empty(),
            "prepare_call_hierarchy should return items"
        );

        let result = client.outgoing_calls(&items[0]).await?;
        let calls = result.as_array().expect("outgoing_calls returns array");
        assert!(
            !calls.is_empty(),
            "outgoing_calls from caller should find callee"
        );
        assert!(
            calls.iter().any(|c| c
                .get("to")
                .and_then(|t| t.get("name"))
                .and_then(Value::as_str)
                == Some("callee")),
            "outgoing_calls should contain callee: {result}"
        );

        client.shutdown().await?;
        Ok(())
    }

    /// Verifies that `prepare_type_hierarchy` returns hierarchy items.
    #[tokio::test]
    async fn prepare_type_hierarchy_returns_items() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("hier.{MOCK_LANG}"));
        let content = "interface Animal\nclass Dog : Animal\n";
        std::fs::write(&script, content)?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client.did_open(&uri, MOCK_LANG, 1, content).await?;

        let result = client.prepare_type_hierarchy(&uri, 0, 10).await?;
        let items = result.as_array().expect("type hierarchy returns array");
        assert!(
            !items.is_empty(),
            "prepare_type_hierarchy should return items"
        );
        assert_eq!(
            items[0].get("name").and_then(Value::as_str),
            Some("Animal"),
            "type hierarchy item should be named Animal"
        );

        client.shutdown().await?;
        Ok(())
    }

    /// Verifies that `code_action` returns quickfix actions.
    #[tokio::test]
    async fn code_action_returns_actions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("action.{MOCK_LANG}"));
        std::fs::write(&script, "let x\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client.did_open(&uri, MOCK_LANG, 1, "let x\n").await?;

        let diagnostic = json!({
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            },
            "severity": 2,
            "source": "mockls",
            "message": "test error"
        });

        let result = client.code_action(&uri, 0, 0, 0, 1, &[diagnostic]).await?;
        let actions = result.as_array().expect("code_action returns array");
        assert!(
            actions
                .iter()
                .any(|a| a.get("kind").and_then(Value::as_str) == Some("quickfix")),
            "code_action should include a quickfix: {result}"
        );

        client.shutdown().await?;
        Ok(())
    }

    /// Verifies that `get_diagnostics` returns cached diagnostics.
    ///
    /// mockls publishes diagnostics on `didOpen`. We poll until they
    /// appear in the cache, then verify `get_diagnostics` returns them.
    #[tokio::test]
    async fn get_diagnostics_returns_cached() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join(format!("diag.{MOCK_LANG}"));
        std::fs::write(&script, "let x\n")?;

        let (mut client, _dir) = spawn_and_init(&[]).await?;

        let uri = format!("file://{}", script.display());
        client.did_open(&uri, MOCK_LANG, 1, "let x\n").await?;

        // Poll until diagnostics arrive (mockls publishes on didOpen)
        let mut found = false;
        for _ in 0..50 {
            let diags = client.get_diagnostics(&uri);
            if !diags.is_empty() {
                found = true;
                assert!(
                    diags
                        .iter()
                        .any(|d| d.get("source").and_then(Value::as_str) == Some("mockls")),
                    "diagnostics should come from mockls: {diags:?}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(found, "diagnostics should appear in cache after didOpen");

        // clear_diagnostics_for removes the cached entry
        client.clear_diagnostics_for(&[&uri]);
        let after_clear = client.get_diagnostics(&uri);
        assert!(
            after_clear.is_empty(),
            "cache should be empty after clear: {after_clear:?}"
        );

        client.shutdown().await?;
        Ok(())
    }
}
