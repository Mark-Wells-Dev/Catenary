// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! A configurable mock LSP server for testing.
//!
//! Speaks the LSP protocol over stdin/stdout using Content-Length framed
//! JSON-RPC. CLI flags control capabilities, timing, and failure modes.
//! No tokio — uses `std::thread` for deferred notifications.
//!
//! Code actions: by default, returns one quickfix action per diagnostic
//! (source "mockls") plus a `refactor` action (to exercise kind filtering).
//! `--no-code-actions` omits the `codeActionProvider` capability entirely.
//! `--multi-fix` returns two quickfix actions per diagnostic.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Mock LSP server for integration testing.
#[derive(Parser, Debug)]
#[command(name = "mockls")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "CLI flags are inherently boolean"
)]
struct Args {
    /// Language name. Used as the file extension for --scan-roots filtering.
    #[arg()]
    name: String,

    /// Advertise workspace folder support with change notifications.
    #[arg(long)]
    workspace_folders: bool,

    /// Emit progress begin/end after initialized (milliseconds).
    #[arg(long, default_value_t = 0)]
    indexing_delay: u64,

    /// Sleep before every response (milliseconds).
    #[arg(long, default_value_t = 0)]
    response_delay: u64,

    /// Delay before publishing diagnostics (milliseconds).
    #[arg(long, default_value_t = 0)]
    diagnostics_delay: u64,

    /// Never publish push diagnostics (`textDocument/publishDiagnostics`).
    #[arg(long)]
    no_push_diagnostics: bool,

    /// Publish an EMPTY diagnostics set (`"diagnostics": []`) on the
    /// didOpen/didChange/didSave push path instead of the mock diagnostic —
    /// a push server's explicit, evidence-backed clean (misc 153; the
    /// push-only Lattice contract). Progress and flycheck publishes are
    /// unaffected.
    #[arg(long)]
    push_empty: bool,

    /// Advertise `diagnosticProvider` and handle `textDocument/diagnostic`
    /// pull requests.
    #[arg(long)]
    pull_diagnostics: bool,

    /// Advertise `diagnosticProvider.workspaceDiagnostics` and serve
    /// `workspace/diagnostic` (whole-workspace pull, LSP 3.17). Implies a
    /// `diagnosticProvider`. A scanned document is reported dirty (one error
    /// diagnostic) when its content contains the marker `DIRTY`, else clean
    /// (a `full` report with empty items) — so a test can shape the
    /// dirty/clean mix a `catenary diagnostics .` receipt collapses. Pair with
    /// `--scan-roots` so the project model is populated without per-file opens.
    #[arg(long)]
    workspace_diagnostics: bool,

    /// Return an error for `textDocument/diagnostic` pull requests.
    /// Used with `--pull-diagnostics` to test runtime downgrade behavior.
    #[arg(long)]
    fail_pull: bool,

    /// Only publish diagnostics on `didSave`, not `didOpen`/`didChange`.
    #[arg(long)]
    diagnostics_on_save: bool,

    /// Close stdout after n responses (simulate crash).
    #[arg(long)]
    drop_after: Option<u64>,

    /// Never respond to this method (repeatable).
    #[arg(long)]
    hang_on: Vec<String>,

    /// Return `InternalError` for this method (repeatable).
    #[arg(long)]
    fail_on: Vec<String>,

    /// Exit the process (code 1) the instant this method is received, before
    /// handling it — simulates a server that dies mid-run (repeatable). Unlike
    /// `--drop-after`, which counts responses, this is keyed on a specific
    /// method, so the death lands at a deterministic point in the pipeline
    /// (e.g. `--die-on textDocument/didSave` dies during the batch's post-save
    /// settle, before any diagnostic is retrieved → the file resolves
    /// `NoResults`). With `--die-once-file`, only the first process arms.
    #[arg(long)]
    die_on: Vec<String>,

    /// Gate `--die-on` to the FIRST process that creates this marker file:
    /// that process dies as configured; any later process (a respawn) sees the
    /// marker already present and runs healthy. Models a transient crash that a
    /// single in-run respawn recovers. Without this flag, every process arms
    /// `--die-on` (a server that dies at every spawn — twice-dead).
    #[arg(long)]
    die_once_file: Option<String>,

    /// Send workspace/configuration request after initialize.
    #[arg(long)]
    send_configuration_request: bool,

    /// Include `version` field in `publishDiagnostics` notifications.
    #[arg(long)]
    publish_version: bool,

    /// Send progress tokens around diagnostic computation on `didChange`
    /// (simulates cargo clippy progress).
    #[arg(long)]
    progress_on_change: bool,

    /// Burn CPU for N milliseconds after `didChange` without sending any
    /// notifications (simulates a server doing work without progress).
    #[arg(long)]
    cpu_busy: Option<u64>,

    /// Command to spawn on `didSave` (simulates flycheck/cargo check).
    /// Wraps the subprocess in a `$/progress` Begin/End bracket.
    /// Use with mockc to create the real scheduling pattern:
    /// `--flycheck-command "mockc --ticks 20"`
    #[arg(long)]
    flycheck_command: Option<String>,

    /// Include `textDocumentSync.save` in `ServerCapabilities`.
    /// Required for the server to receive `textDocument/didSave`.
    #[arg(long)]
    advertise_save: bool,

    /// Write every received notification method to a JSONL file.
    /// Each line is `{"method":"...","uri":"..."}` (uri if available).
    /// Tests read after shutdown to verify notification delivery.
    #[arg(long)]
    notification_log: Option<String>,

    /// Append this process's PID to the `--notification-log` and
    /// `--request-log` paths (`<path>.<pid>`).
    ///
    /// Under the per-root server architecture, one language spawns one instance
    /// per tracked root, and every instance is handed the SAME `CATENARY_SERVERS`
    /// log path. Multiple instances appending to one file produce byte-interleaved
    /// (torn) JSONL lines that no reader can reliably parse — which makes a
    /// contention-resistant signal poll impossible. With this flag each instance
    /// writes its OWN file (one writer ⇒ no torn lines); a test reads all files
    /// matching `<path>.*` and merges them (see `common::merge_logs`).
    #[arg(long)]
    log_pid_suffix: bool,

    /// Write every handled request method to a JSONL file.
    /// Each line is `{"method":"..."}`. Tests read after shutdown to verify
    /// which requests the daemon issued (e.g. a cold `textDocument/documentSymbol`).
    #[arg(long)]
    request_log: Option<String>,

    /// Return `ContentModified` (-32801) on the first `textDocument/definition`
    /// request, then succeed on retry. Tests the retry path.
    #[arg(long)]
    content_modified_once: bool,

    /// Burn CPU for N milliseconds on `workspace/didChangeWorkspaceFolders`.
    /// No progress tokens are sent. Tests `wait_ready` failure detection.
    #[arg(long)]
    cpu_on_workspace_change: Option<u64>,

    /// Burn CPU for N milliseconds on `initialized` notification (before
    /// indexing simulation). Tests warmup observation in `is_ready()`.
    #[arg(long)]
    cpu_on_initialized: Option<u64>,

    /// Write the `initialize` request params JSON to the specified file.
    /// Tests can read this to verify client capabilities.
    #[arg(long)]
    log_init_params: Option<String>,

    /// Override the number of ticks passed to the flycheck subprocess.
    /// Appends `--ticks <N>` to the flycheck command args.
    #[arg(long)]
    flycheck_ticks: Option<u64>,

    /// Run the flycheck subprocess WITHOUT a `$/progress` Begin/End bracket
    /// (bug 28). The child burns CPU while the server lifecycle stays `Healthy`,
    /// so the settle's failure budget accrues on the child's (tree-summed) CPU
    /// — reproducing a settle that bails (`BudgetExhausted`) on legitimate
    /// flycheck work that has no open progress bracket.
    #[arg(long)]
    flycheck_no_progress: bool,

    /// Scan workspace roots on initialize and workspace folder changes.
    /// Indexes all text files into `documents`, making them visible to
    /// `workspace/symbol` without a prior `didOpen`.
    #[arg(long)]
    scan_roots: bool,

    /// Never return code actions (omit `codeActionProvider` capability).
    #[arg(long)]
    no_code_actions: bool,

    /// Omit `renameProvider` from capabilities.
    #[arg(long)]
    no_rename: bool,

    /// Omit `typeHierarchyProvider` from capabilities.
    #[arg(long)]
    no_type_hierarchy: bool,

    /// Return multiple quickfix actions per diagnostic.
    #[arg(long)]
    multi_fix: bool,

    /// Advertise `workspaceSymbol/resolve` support. When set, `workspace/symbol`
    /// returns URI-only locations (no range) and `workspaceSymbol/resolve`
    /// returns full locations.
    #[arg(long)]
    resolve_provider: bool,

    /// Return empty results for `workspace/symbol` with empty query.
    /// Forces the fallback to per-query lookup.
    #[arg(long)]
    no_empty_query: bool,

    /// Send `client/registerCapability` after `initialized` to register a
    /// file watcher. The glob pattern defaults to `**/*`; override with
    /// `--watcher-glob`.
    #[arg(long)]
    register_file_watchers: bool,

    /// Glob pattern for the file watcher registration (default `**/*`).
    /// Only meaningful with `--register-file-watchers`.
    #[arg(long, default_value = "**/*")]
    watcher_glob: String,

    /// Restrict the registered file watcher to a specific `WatchKind` bitmask
    /// (1=Create, 2=Change, 4=Delete; default 7=all). Only meaningful with
    /// `--register-file-watchers`.
    #[arg(long)]
    watcher_kind: Option<u8>,

    /// Include the number of currently-open documents in the diagnostic
    /// message. Enables tests to verify that the batch pipeline opens all
    /// files before settling (batch: "N open" vs sequential: "1 open").
    #[arg(long)]
    report_open_count: bool,

    /// Reject initialization when no workspace root is provided (rootUri
    /// is null and workspaceFolders is empty). Returns an `InitializeError`
    /// to test single-file mode negative caching.
    #[arg(long)]
    reject_null_workspace: bool,

    /// Write this message to stderr after receiving `initialized`.
    /// Used to test stderr capture.
    #[arg(long)]
    stderr_message: Option<String>,

    /// Write a line of N repeated 'x' characters to stderr after
    /// `initialized`. Used to test truncation boundaries.
    #[arg(long)]
    stderr_length: Option<usize>,

    /// Include the value of the named environment variable in the
    /// `serverInfo.version` field of the initialize response.
    /// Used to verify that per-server `env` config reaches the process.
    #[arg(long)]
    report_env: Option<String>,

    /// Publish an EXTRA diagnostic alongside the default one, with a custom
    /// `source` and `code` (repeatable). Lets a single mockls instance emit
    /// overlapping multi-source diagnostics — the shape rust-analyzer produces
    /// when its native HIR analysis and flycheck both publish for one file
    /// (misc 115, bug 42; mockls cannot reproduce RA's macro engine).
    ///
    /// Format: `source|code|message` (pipe-delimited). `code` may be empty.
    /// Severity is always error (1). Example:
    /// `--extra-diagnostic 'rust-analyzer|E0107|phantom arity error'`.
    #[arg(long)]
    extra_diagnostic: Vec<String>,
}

/// A JSON-RPC request.
#[derive(Debug, Deserialize)]
struct Request {
    #[allow(dead_code, reason = "Required by JSON-RPC protocol")]
    jsonrpc: String,
    id: Option<Value>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
}

/// A JSON-RPC response.
#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

/// Thread-safe writer handle. Wraps `std::io::Stdout` for production,
/// or a shared `Vec<u8>` for tests.
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Create a writer that forwards to stdout.
fn stdout_writer() -> Writer {
    Arc::new(Mutex::new(Box::new(std::io::stdout())))
}

#[cfg(test)]
fn buffer_writer() -> (Writer, Arc<Mutex<Vec<u8>>>) {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer: Box<dyn Write + Send> = Box::new(SharedVecWriter(buf.clone()));
    (Arc::new(Mutex::new(writer)), buf)
}

/// Write adapter for `Arc<Mutex<Vec<u8>>>` used in tests.
#[cfg(test)]
struct SharedVecWriter(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedVecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Shared state for the mock server.
struct MockServer {
    args: Args,
    documents: BTreeMap<String, String>,
    /// Mock type map: `symbol_name → type_name` extracted from `: TypeName` annotations.
    types: HashMap<String, String>,
    /// Import map: `(document_uri, imported_name) → source_file_fragment`.
    /// Parsed from `from <file> import <name>` lines.
    imports: HashMap<(String, String), String>,
    /// Tracks the document version from `didOpen`/`didChange` per URI.
    versions: HashMap<String, i32>,
    response_count: u64,
    writer: Writer,
    shutdown_flag: Arc<AtomicBool>,
    next_request_id: Arc<AtomicU64>,
    /// Optional notification log file for test verification.
    notification_log: Option<std::fs::File>,
    /// Optional request log file for test verification.
    request_log: Option<std::fs::File>,
    /// Whether the first definition request has been seen (for `--content-modified-once`).
    definition_failed_once: bool,
    /// Workspace roots parsed from `initialize` params.
    workspace_roots: Vec<String>,
    /// Whether `--die-on` is armed for THIS process (decision 027 recovery
    /// tests). Always `true` when `--die-once-file` is unset; when set, `true`
    /// only for the process that created the marker (the first life).
    die_armed: bool,
}

impl MockServer {
    fn new(args: Args, writer: Writer) -> Self {
        // With `--log-pid-suffix`, each per-root instance writes its OWN file
        // (`<path>.<pid>`) so concurrent instances of one language never share a
        // file and tear each other's JSONL lines. The test merges `<path>.*`.
        let log_path = |path: &str| -> String {
            if args.log_pid_suffix {
                format!("{path}.{}", std::process::id())
            } else {
                path.to_string()
            }
        };

        let notification_log = args
            .notification_log
            .as_ref()
            .and_then(|path| std::fs::File::create(log_path(path)).ok());

        let request_log = args
            .request_log
            .as_ref()
            .and_then(|path| std::fs::File::create(log_path(path)).ok());

        // Arm `--die-on` for this process. With `--die-once-file`, only the
        // process that wins the atomic create (the first life) arms; a later
        // respawn sees the marker and runs healthy, so one in-run respawn
        // recovers. Without the marker, every process arms (twice-dead).
        let die_armed = args.die_once_file.as_ref().is_none_or(|marker| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(marker)
                .is_ok()
        });

        Self {
            args,
            documents: BTreeMap::new(),
            types: HashMap::new(),
            imports: HashMap::new(),
            versions: HashMap::new(),
            response_count: 0,
            writer,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            next_request_id: Arc::new(AtomicU64::new(1)),
            notification_log,
            request_log,
            definition_failed_once: false,
            workspace_roots: Vec::new(),
            die_armed,
        }
    }

    /// Recursively scans a directory, indexing `.mock` files into `self.documents`.
    fn scan_directory(&mut self, dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip hidden directories
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
                self.scan_directory(&path);
            } else if path.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some(self.args.name.as_str())
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let abs = path.to_string_lossy();
                let uri = format!("file://{abs}");
                self.documents.insert(uri, content);
            }
        }
    }

    /// Rebuilds the type map from all open documents.
    fn rebuild_types(&mut self) {
        self.types.clear();
        for content in self.documents.values() {
            self.types.extend(extract_types(content));
        }
    }

    /// Rebuilds the import map from all open documents.
    /// Parses `from <file> import <name>` lines.
    fn rebuild_imports(&mut self) {
        self.imports.clear();
        for (uri, content) in &self.documents {
            for line_text in content.lines() {
                let trimmed = line_text.trim_start();
                if let Some(rest) = trimmed.strip_prefix("from ") {
                    let mut parts = rest.split_whitespace();
                    let Some(file_fragment) = parts.next() else {
                        continue;
                    };
                    if parts.next() != Some("import") {
                        continue;
                    }
                    let Some(name) = parts.next() else {
                        continue;
                    };
                    self.imports
                        .insert((uri.clone(), name.to_string()), file_fragment.to_string());
                }
            }
        }
    }

    /// Run the server, reading from the given reader.
    fn run(&mut self, reader: &mut dyn Read) {
        let mut buffer = Vec::new();
        let mut temp = [0u8; 4096];

        loop {
            if self.shutdown_flag.load(Ordering::SeqCst) {
                break;
            }

            match reader.read(&mut temp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buffer.extend_from_slice(&temp[..n]),
            }

            while let Some((message, consumed)) = try_parse_message(&buffer) {
                buffer.drain(..consumed);

                let Ok(request) = serde_json::from_str::<Request>(&message) else {
                    continue;
                };

                self.handle_message(request);
            }
        }
    }

    fn handle_message(&mut self, request: Request) {
        let Some(method) = request.method.clone() else {
            return;
        };

        // `--die-on`: exit the instant this method is received, before handling
        // it (so no diagnostic is published/retrieved for it). Armed per-process
        // by `--die-once-file` (see `MockServer::new`).
        if self.die_armed && self.args.die_on.iter().any(|m| m == &method) {
            std::process::exit(1);
        }

        if request.id.is_some() {
            self.handle_request(&method, request);
        } else {
            self.handle_notification(&method, &request.params);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Method dispatch requires handling many LSP methods"
    )]
    fn handle_request(&mut self, method: &str, request: Request) {
        let Some(id) = request.id else { return };

        // Log request method if configured (mirrors notification_log).
        if let Some(ref mut log) = self.request_log {
            let _ = writeln!(log, "{}", serde_json::json!({ "method": method }));
        }

        // Check hang_on — never respond
        if self.args.hang_on.iter().any(|m| m == method) {
            return;
        }

        // Response delay
        if self.args.response_delay > 0 {
            std::thread::sleep(Duration::from_millis(self.args.response_delay));
        }

        // Check fail_on — return `InternalError`
        if self.args.fail_on.iter().any(|m| m == method) {
            self.send_response(&Response {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(RpcError {
                    code: -32603,
                    message: format!("mockls: configured to fail on {method}"),
                }),
            });
            return;
        }

        let result = match method {
            "initialize" => {
                if let Some(ref path) = self.args.log_init_params {
                    let json = serde_json::to_string_pretty(&request.params).unwrap_or_default();
                    let _ = std::fs::write(path, json);
                }
                // Reject null-workspace initialization if configured.
                if self.args.reject_null_workspace {
                    let root_null = request.params.get("rootUri").is_none_or(Value::is_null);
                    let folders_empty = request
                        .params
                        .get("workspaceFolders")
                        .and_then(Value::as_array)
                        .is_none_or(Vec::is_empty);
                    if root_null && folders_empty {
                        self.send_response(&Response {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: None,
                            error: Some(RpcError {
                                code: -32002,
                                message: "mockls: workspace root required".to_string(),
                            }),
                        });
                        return;
                    }
                }
                Some(self.handle_initialize(&request.params))
            }
            "shutdown" => Some(Value::Null),
            "textDocument/hover" => self.handle_hover(&request.params),
            "textDocument/definition" => {
                if self.args.content_modified_once && !self.definition_failed_once {
                    self.definition_failed_once = true;
                    self.send_response(&Response {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(RpcError {
                            code: -32801,
                            message: "ContentModified".to_string(),
                        }),
                    });
                    return;
                }
                self.handle_definition(&request.params)
            }
            "textDocument/typeDefinition" => self.handle_type_definition(&request.params),
            "textDocument/references" => self.handle_references(&request.params),
            "textDocument/implementation" => self.handle_implementation(&request.params),
            "textDocument/documentSymbol" => self.handle_document_symbols(&request.params),
            "workspace/symbol" => Some(self.handle_workspace_symbols(&request.params)),
            "workspaceSymbol/resolve" => self.handle_workspace_symbol_resolve(&request.params),
            "textDocument/prepareCallHierarchy" => {
                self.handle_call_hierarchy_prepare(&request.params)
            }
            "callHierarchy/incomingCalls" => self.handle_incoming_calls(&request.params),
            "textDocument/prepareTypeHierarchy" => {
                self.handle_type_hierarchy_prepare(&request.params)
            }
            "typeHierarchy/subtypes" => self.handle_type_hierarchy_subtypes(&request.params),
            "textDocument/diagnostic" => {
                if self.args.fail_pull {
                    self.send_response(&Response {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(RpcError {
                            code: -32603,
                            message: "mockls: pull diagnostics failure (--fail-pull)".to_string(),
                        }),
                    });
                    return;
                }
                Some(self.handle_pull_diagnostics(&request.params))
            }
            "workspace/diagnostic" => Some(self.handle_workspace_diagnostics()),
            "textDocument/codeAction" => Some(self.handle_code_action(&request.params)),
            "textDocument/prepareRename" => self.handle_prepare_rename(&request.params),
            "callHierarchy/outgoingCalls" => self.handle_outgoing_calls(&request.params),
            "typeHierarchy/supertypes" => self.handle_type_hierarchy_supertypes(&request.params),
            _ => {
                self.send_response(&Response {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: None,
                    error: Some(RpcError {
                        code: -32601,
                        message: format!("mockls: method not found: {method}"),
                    }),
                });
                return;
            }
        };

        self.send_response(&Response {
            jsonrpc: "2.0".to_string(),
            id,
            result,
            error: None,
        });

        if method == "initialize" && self.args.send_configuration_request {
            self.send_configuration_request();
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Notification dispatch handles many LSP methods with scan-roots logic"
    )]
    fn handle_notification(&mut self, method: &str, params: &Value) {
        // Log notification if configured
        if let Some(ref mut log) = self.notification_log {
            let entry = if method == "workspace/didChangeWatchedFiles" {
                let changes = params
                    .get("changes")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                serde_json::json!({"method": method, "changes": changes})
            } else {
                let uri = params
                    .get("textDocument")
                    .and_then(|td| td.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                serde_json::json!({"method": method, "uri": uri})
            };
            let _ = writeln!(log, "{entry}");
        }

        match method {
            "initialized" => {
                if let Some(ref msg) = self.args.stderr_message {
                    let _ = writeln!(std::io::stderr(), "{msg}");
                }
                if let Some(n) = self.args.stderr_length {
                    let line: String = "x".repeat(n);
                    let _ = writeln!(std::io::stderr(), "{line}");
                }
                if let Some(busy_ms) = self.args.cpu_on_initialized {
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(busy_ms) {
                        std::hint::spin_loop();
                    }
                }
                if self.args.register_file_watchers {
                    self.send_register_file_watchers();
                }
                if self.args.indexing_delay > 0 {
                    self.start_indexing_simulation();
                }
            }
            "textDocument/didOpen" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    let text = td.get("text").and_then(Value::as_str).unwrap_or_default();
                    let version = td
                        .get("version")
                        .and_then(Value::as_i64)
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or(1);
                    self.documents.insert(uri.to_string(), text.to_string());
                    self.versions.insert(uri.to_string(), version);
                    self.rebuild_types();
                    self.rebuild_imports();

                    if !self.args.no_push_diagnostics && !self.args.diagnostics_on_save {
                        if self.args.report_open_count {
                            // Simulate cross-file reanalysis: re-publish
                            // diagnostics for all open documents so every
                            // file's cached diagnostics reflect the current
                            // open count.
                            let uris: Vec<String> = self.documents.keys().cloned().collect();
                            for u in &uris {
                                self.publish_diagnostics(u);
                            }
                        } else {
                            self.publish_diagnostics(uri);
                        }
                    }
                }
            }
            "textDocument/didChange" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    let version = td
                        .get("version")
                        .and_then(Value::as_i64)
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or(1);
                    self.versions.insert(uri.to_string(), version);
                    if let Some(text) = params
                        .get("contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.last())
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                    {
                        self.documents.insert(uri.to_string(), text.to_string());
                        self.rebuild_types();
                        self.rebuild_imports();
                    }

                    // Simulate CPU-bound work without any notifications
                    if let Some(busy_ms) = self.args.cpu_busy {
                        let start = std::time::Instant::now();
                        while start.elapsed() < Duration::from_millis(busy_ms) {
                            std::hint::spin_loop();
                        }
                    }

                    if self.args.progress_on_change {
                        self.simulate_progress_around_diagnostics(uri);
                    } else if !self.args.no_push_diagnostics && !self.args.diagnostics_on_save {
                        self.publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didSave" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    if let Some(ref cmd) = self.args.flycheck_command {
                        self.run_flycheck(uri, cmd);
                    } else if !self.args.no_push_diagnostics {
                        self.publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(td) = params.get("textDocument") {
                    let uri = td.get("uri").and_then(Value::as_str).unwrap_or_default();
                    // LSP-faithful close: drop the in-memory overlay but retain
                    // disk-backed documents. `scan_directory` builds URIs as
                    // `format!("file://{abs}")`, so reverse it by stripping the
                    // `file://` prefix and re-reading the file if it still exists
                    // on disk. A purely synthetic `didOpen` doc (no real file) is
                    // removed as before.
                    let disk_content = uri
                        .strip_prefix("file://")
                        .filter(|path| std::path::Path::new(path).is_file())
                        .and_then(|path| std::fs::read_to_string(path).ok());
                    if let Some(content) = disk_content {
                        self.documents.insert(uri.to_string(), content);
                        self.rebuild_types();
                        self.rebuild_imports();
                    } else {
                        self.documents.remove(uri);
                    }
                }
            }
            "exit" => {
                self.shutdown_flag.store(true, Ordering::SeqCst);
                std::process::exit(0);
            }
            "workspace/didChangeWorkspaceFolders" => {
                if let Some(busy_ms) = self.args.cpu_on_workspace_change {
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(busy_ms) {
                        std::hint::spin_loop();
                    }
                }

                if self.args.scan_roots
                    && let Some(event) = params.get("event")
                {
                    // Remove documents from removed folders
                    if let Some(removed) = event.get("removed").and_then(Value::as_array) {
                        for folder in removed {
                            if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                                let path = uri.strip_prefix("file://").unwrap_or(uri);
                                self.workspace_roots.retain(|r| r != path);
                                let prefix = format!("file://{path}");
                                self.documents.retain(|k, _| !k.starts_with(&prefix));
                            }
                        }
                    }
                    // Scan added folders
                    if let Some(added) = event.get("added").and_then(Value::as_array) {
                        for folder in added {
                            if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                                let path = uri.strip_prefix("file://").unwrap_or(uri);
                                if !self.workspace_roots.contains(&path.to_string()) {
                                    self.workspace_roots.push(path.to_string());
                                }
                                self.scan_directory(std::path::Path::new(path));
                            }
                        }
                    }
                    self.rebuild_types();
                    self.rebuild_imports();
                }
            }
            // All other notifications are silently accepted
            _ => {}
        }
    }

    fn handle_initialize(&mut self, params: &Value) -> Value {
        // Parse workspace roots from initialize params
        let mut roots = Vec::new();
        if let Some(uri) = params.get("rootUri").and_then(Value::as_str) {
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            if !path.is_empty() {
                roots.push(path.to_string());
            }
        }
        if let Some(folders) = params.get("workspaceFolders").and_then(Value::as_array) {
            for folder in folders {
                if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                    let path = uri.strip_prefix("file://").unwrap_or(uri);
                    if !path.is_empty() && !roots.contains(&path.to_string()) {
                        roots.push(path.to_string());
                    }
                }
            }
        }

        if self.args.scan_roots {
            for root in &roots {
                self.scan_directory(std::path::Path::new(root));
            }
            self.rebuild_types();
            self.rebuild_imports();
        }
        self.workspace_roots = roots;

        // Record this instance's primary workspace root as the FIRST line of the
        // notification log. Under the per-root server architecture one language
        // spawns one instance per tracked root; `--log-pid-suffix` already gives
        // each instance its own `<base>.<pid>` file, and this marker lets a test
        // identify WHICH root an instance is scoped to (so it can assert against
        // the right instance's log — e.g. the `parent`-scoped vs `parent/sub`-
        // scoped instance of one language). The leading `__instance_root` method
        // is ignored by the `workspace/didChangeWatchedFiles` parsers.
        if let Some(ref mut log) = self.notification_log {
            let root = self.workspace_roots.first().map_or("", String::as_str);
            let _ = writeln!(
                log,
                "{}",
                serde_json::json!({ "method": "__instance_root", "uri": root })
            );
        }

        let mut text_doc_sync = serde_json::json!({
            "openClose": true,
            "change": 1
        });
        if self.args.advertise_save {
            text_doc_sync["save"] = serde_json::json!({ "includeText": false });
        }

        let workspace_symbol_value = if self.args.resolve_provider {
            serde_json::json!({ "resolveProvider": true })
        } else {
            serde_json::json!(true)
        };

        let mut capabilities = serde_json::json!({
            "hoverProvider": true,
            "definitionProvider": true,
            "typeDefinitionProvider": true,
            "referencesProvider": true,
            "implementationProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": workspace_symbol_value,
            "callHierarchyProvider": true,
            "textDocumentSync": text_doc_sync
        });

        if !self.args.no_type_hierarchy {
            capabilities["typeHierarchyProvider"] = serde_json::json!(true);
        }
        if !self.args.no_rename {
            capabilities["renameProvider"] = serde_json::json!({ "prepareProvider": true });
        }
        if !self.args.no_code_actions {
            capabilities["codeActionProvider"] = serde_json::json!(true);
        }

        if self.args.pull_diagnostics || self.args.workspace_diagnostics {
            // `workspaceDiagnostics` gates the whole-workspace pull; with it we
            // also advertise `interFileDependencies` (the server reasons across
            // files), matching a real workspace-diagnostic server.
            capabilities["diagnosticProvider"] = serde_json::json!({
                "interFileDependencies": self.args.workspace_diagnostics,
                "workspaceDiagnostics": self.args.workspace_diagnostics
            });
        }

        if self.args.workspace_folders {
            capabilities["workspace"] = serde_json::json!({
                "workspaceFolders": {
                    "supported": true,
                    "changeNotifications": true
                }
            });
        }

        let mut result = serde_json::json!({ "capabilities": capabilities });
        if let Some(ref var_name) = self.args.report_env {
            let value = std::env::var(var_name).unwrap_or_default();
            result["serverInfo"] = serde_json::json!({
                "name": "mockls",
                "version": value
            });
        }
        result
    }

    fn handle_hover(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_symbol_name(content, line, col)?;

        Some(serde_json::json!({
            "contents": {
                "kind": "markdown",
                "value": format!("```\n{word}\n```")
            }
        }))
    }

    /// Returns null for declaration keywords (`fn`, `struct`, `class`, etc.)
    /// and a range for everything else. The keyword list is specifically
    /// words that introduce definitions — general keywords like `if`, `for`,
    /// `while` are NOT filtered and will return a range via the
    /// first-occurrence fallback.
    fn handle_prepare_rename(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_word(content, line, col)?;

        let keywords = [
            "fn",
            "function",
            "def",
            "let",
            "const",
            "var",
            "struct",
            "class",
            "enum",
            "interface",
            "trait",
            "mod",
            "module",
            "type",
            "method",
            "field",
        ];
        if keywords.contains(&word.as_str()) {
            return None;
        }

        let line_text = content.lines().nth(line)?;
        let bytes = line_text.as_bytes();
        let start = (0..=col)
            .rev()
            .find(|&i| !is_word_char(bytes[i]))
            .map_or(0, |i| i + 1);
        let end = (col..bytes.len())
            .find(|&i| !is_word_char(bytes[i]))
            .unwrap_or(bytes.len());

        Some(serde_json::json!({
            "range": {
                "start": { "line": line, "character": start },
                "end": { "line": line, "character": end }
            },
            "placeholder": word
        }))
    }

    fn handle_definition(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_word(content, line, col)?;

        let def_patterns = [
            format!("fn {word}"),
            format!("function {word}"),
            format!("def {word}"),
            format!("let {word}"),
            format!("const {word}"),
            format!("var {word}"),
            format!("struct {word}"),
            format!("class {word}"),
            format!("enum {word}"),
            format!("interface {word}"),
            format!("trait {word}"),
            format!("mod {word}"),
            format!("module {word}"),
            format!("type {word}"),
            format!("method {word}"),
            format!("field {word}"),
        ];

        for (line_idx, line_text) in content.lines().enumerate() {
            for pattern in &def_patterns {
                if let Some(col_idx) = line_text.find(pattern.as_str()) {
                    return Some(location_json(
                        uri,
                        line_idx,
                        col_idx,
                        col_idx + pattern.len(),
                    ));
                }
            }
        }

        // Import-scoped resolution: if this file imports the word, search
        // only the source document for a definition pattern.
        if let Some(source_fragment) = self.imports.get(&(uri.to_string(), word.clone())) {
            for (doc_uri, doc_content) in &self.documents {
                if !doc_uri.contains(source_fragment.as_str()) {
                    continue;
                }
                for (line_idx, line_text) in doc_content.lines().enumerate() {
                    for pattern in &def_patterns {
                        if let Some(col_idx) = line_text.find(pattern.as_str()) {
                            return Some(location_json(
                                doc_uri,
                                line_idx,
                                col_idx,
                                col_idx + pattern.len(),
                            ));
                        }
                    }
                }
            }
        }

        // Cross-file: search all other documents for a definition pattern
        for (doc_uri, doc_content) in &self.documents {
            if doc_uri == uri {
                continue;
            }
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                for pattern in &def_patterns {
                    if let Some(col_idx) = line_text.find(pattern.as_str()) {
                        return Some(location_json(
                            doc_uri,
                            line_idx,
                            col_idx,
                            col_idx + pattern.len(),
                        ));
                    }
                }
            }
        }

        // Cross-file fallback: first occurrence in any other document.
        // Checked before current-doc because a cross-file match is more
        // likely to be the real definition than re-finding the word at
        // the cursor position.
        for (doc_uri, doc_content) in &self.documents {
            if doc_uri == uri {
                continue;
            }
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                if let Some(col_idx) = line_text.find(&word) {
                    return Some(location_json(
                        doc_uri,
                        line_idx,
                        col_idx,
                        col_idx + word.len(),
                    ));
                }
            }
        }

        // Last resort: first occurrence in current document
        for (line_idx, line_text) in content.lines().enumerate() {
            if let Some(col_idx) = line_text.find(&word) {
                return Some(location_json(uri, line_idx, col_idx, col_idx + word.len()));
            }
        }

        None
    }

    fn handle_references(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let word = extract_symbol_name(content, line, col)?;

        let mut locations = Vec::new();
        // Cross-file: search all documents for the word
        for (doc_uri, doc_content) in &self.documents {
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                let mut start = 0;
                while let Some(pos) = line_text[start..].find(&word) {
                    let col_idx = start + pos;
                    locations.push(location_json(
                        doc_uri,
                        line_idx,
                        col_idx,
                        col_idx + word.len(),
                    ));
                    start = col_idx + word.len();
                }
            }
        }

        Some(Value::Array(locations))
    }

    /// Goto-implementation: `Location[]` for every type whose declaration
    /// `implements` the queried type (the `implements` keyword only).
    ///
    /// Deliberately distinct from `handle_references` (text-scan occurrences)
    /// and from `handle_type_hierarchy_subtypes` (which spans both `extends`
    /// and `implements`): an `extends`-only subtype is a subtype but not an
    /// implementor, so it is excluded here. Each location points at the
    /// implementing type's name in its declaration.
    fn handle_implementation(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let queried = extract_symbol_name(content, line, col)?;

        let type_keywords: &[&str] = &["struct ", "class ", "interface ", "trait ", "enum "];
        let mut locations = Vec::new();

        for (doc_uri, doc_content) in &self.documents {
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                let trimmed = line_text.trim_start();
                let indent = line_text.len() - trimmed.len();

                for &kw in type_keywords {
                    let Some(after_kw) = trimmed.strip_prefix(kw) else {
                        continue;
                    };
                    let name: String = after_kw
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if name.is_empty() {
                        break;
                    }
                    // Match `implements <queried>` only (NOT `extends`).
                    if let Some(pos) = trimmed.find("implements ") {
                        let after = &trimmed[pos + "implements ".len()..];
                        let target: String = after
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if target == queried {
                            let name_col = indent + kw.len();
                            locations.push(location_json(
                                doc_uri,
                                line_idx,
                                name_col,
                                name_col + name.len(),
                            ));
                        }
                    }
                    break; // Only one keyword prefix can match per line.
                }
            }
        }

        Some(Value::Array(locations))
    }

    fn handle_type_definition(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let name = extract_symbol_name(content, line, col)?;

        // Look up name in type map. If not found, extract types from the
        // current line as a fallback (handles cases where the cursor lands
        // on a keyword and the name resolves but has no global type entry).
        let type_name = self.types.get(&name).cloned().or_else(|| {
            let line_text = content.lines().nth(line)?;
            let line_types = extract_types(line_text);
            line_types.into_values().next()
        })?;

        // Type declaration patterns
        let type_decl_patterns = [
            format!("struct {type_name}"),
            format!("class {type_name}"),
            format!("enum {type_name}"),
            format!("interface {type_name}"),
            format!("trait {type_name}"),
            format!("type {type_name}"),
        ];

        // Search all documents for the type declaration
        for (doc_uri, doc_content) in &self.documents {
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                for pattern in &type_decl_patterns {
                    if let Some(col_idx) = line_text.find(pattern.as_str()) {
                        return Some(location_json(
                            doc_uri,
                            line_idx,
                            col_idx,
                            col_idx + pattern.len(),
                        ));
                    }
                }
            }
        }

        None
    }

    fn handle_call_hierarchy_prepare(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let name = extract_symbol_name(content, line, col)?;
        let line_text = content.lines().nth(line)?;

        let mut item = serde_json::json!({
            "name": name,
            "kind": 12,
            "uri": uri,
            "range": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            },
            "selectionRange": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            }
        });
        if line_text.trim_start().contains("@deprecated") {
            item["tags"] = serde_json::json!([1]);
        }

        Some(serde_json::json!([item]))
    }

    fn handle_incoming_calls(&self, params: &Value) -> Option<Value> {
        let item = params.get("item")?;
        let name = item.get("name")?.as_str()?;
        let def_uri = item.get("uri")?.as_str()?;
        let def_line = item.get("range")?.get("start")?.get("line")?.as_u64()?;

        let mut calls = Vec::new();

        for (doc_uri, content) in &self.documents {
            for (line_idx, line_text) in content.lines().enumerate() {
                if doc_uri == def_uri && line_idx as u64 == def_line {
                    continue;
                }

                if !line_text.contains(name) {
                    continue;
                }

                if let Some((fn_name, fn_line)) = find_enclosing_function(content, line_idx) {
                    let fn_line_text = content.lines().nth(fn_line).unwrap_or("");
                    let mut from_item = serde_json::json!({
                        "name": fn_name,
                        "kind": 12,
                        "uri": doc_uri,
                        "range": {
                            "start": { "line": fn_line, "character": 0 },
                            "end": { "line": fn_line, "character": fn_line_text.len() }
                        },
                        "selectionRange": {
                            "start": { "line": fn_line, "character": 0 },
                            "end": { "line": fn_line, "character": fn_line_text.len() }
                        }
                    });
                    if fn_line_text.trim_start().contains("@deprecated") {
                        from_item["tags"] = serde_json::json!([1]);
                    }
                    calls.push(serde_json::json!({
                        "from": from_item,
                        "fromRanges": [{
                            "start": { "line": line_idx, "character": 0 },
                            "end": { "line": line_idx, "character": line_text.len() }
                        }]
                    }));
                }
            }
        }

        Some(Value::Array(calls))
    }

    fn handle_type_hierarchy_prepare(&self, params: &Value) -> Option<Value> {
        let (uri, line, col) = extract_position(params)?;
        let content = self.documents.get(uri)?;
        let name = extract_symbol_name(content, line, col)?;
        let line_text = content.lines().nth(line)?;

        let trimmed = line_text.trim_start();
        let kind: u32 = if trimmed.starts_with("interface ") || trimmed.starts_with("trait ") {
            11
        } else if trimmed.starts_with("class ") {
            5
        } else if trimmed.starts_with("enum ") {
            10
        } else {
            23
        };

        let mut item = serde_json::json!({
            "name": name,
            "kind": kind,
            "uri": uri,
            "range": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            },
            "selectionRange": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": line_text.len() }
            }
        });
        if trimmed.contains("@deprecated") {
            item["tags"] = serde_json::json!([1]);
        }

        Some(serde_json::json!([item]))
    }

    fn handle_type_hierarchy_subtypes(&self, params: &Value) -> Option<Value> {
        let item = params.get("item")?;
        let parent_name = item.get("name")?.as_str()?;

        let type_keywords: &[(&str, u32)] = &[
            ("struct ", 23),
            ("class ", 5),
            ("interface ", 11),
            ("trait ", 11),
            ("enum ", 10),
        ];
        let mut subtypes = Vec::new();

        for (doc_uri, content) in &self.documents {
            for (line_idx, line_text) in content.lines().enumerate() {
                let trimmed = line_text.trim_start();

                // Check if this line declares a type that extends/implements the parent
                let mut is_subtype = false;
                let mut type_name = String::new();
                let mut kind: u32 = 5;

                for &(kw, kw_kind) in type_keywords {
                    if let Some(after_kw) = trimmed.strip_prefix(kw) {
                        let name: String = after_kw
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if name.is_empty() {
                            continue;
                        }
                        // Check for `extends <parent>` or `implements <parent>`
                        for pattern in &["extends ", "implements "] {
                            if let Some(pos) = trimmed.find(pattern) {
                                let after = &trimmed[pos + pattern.len()..];
                                let target: String = after
                                    .chars()
                                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                                    .collect();
                                if target == parent_name {
                                    is_subtype = true;
                                    type_name.clone_from(&name);
                                    kind = kw_kind;
                                    break;
                                }
                            }
                        }
                        break; // Only one keyword prefix can match per line
                    }
                }

                if is_subtype {
                    let mut item_json = serde_json::json!({
                        "name": type_name,
                        "kind": kind,
                        "uri": doc_uri,
                        "range": {
                            "start": { "line": line_idx, "character": 0 },
                            "end": { "line": line_idx, "character": line_text.len() }
                        },
                        "selectionRange": {
                            "start": { "line": line_idx, "character": 0 },
                            "end": { "line": line_idx, "character": line_text.len() }
                        }
                    });
                    if trimmed.contains("@deprecated") {
                        item_json["tags"] = serde_json::json!([1]);
                    }
                    subtypes.push(item_json);
                }
            }
        }

        Some(Value::Array(subtypes))
    }

    fn handle_type_hierarchy_supertypes(&self, params: &Value) -> Option<Value> {
        let item = params.get("item")?;
        let name = item.get("name")?.as_str()?;

        let mut supertypes = Vec::new();

        // Find the declaration line for this type and look for extends/implements
        for (doc_uri, content) in &self.documents {
            for (line_idx, line_text) in content.lines().enumerate() {
                let trimmed = line_text.trim_start();
                // Check if this line declares the type we're looking for
                let declares_name = ["struct ", "class ", "interface ", "trait ", "enum "]
                    .iter()
                    .any(|kw| {
                        trimmed.strip_prefix(kw).is_some_and(|after| {
                            let decl_name: String = after
                                .chars()
                                .take_while(|c| c.is_alphanumeric() || *c == '_')
                                .collect();
                            decl_name == name
                        })
                    });

                if !declares_name {
                    continue;
                }

                // Look for `extends <Type>` or `implements <Type>` on this line
                for pattern in &["extends ", "implements "] {
                    if let Some(pos) = trimmed.find(pattern) {
                        let after = &trimmed[pos + pattern.len()..];
                        let parent_name: String = after
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if parent_name.is_empty() {
                            continue;
                        }

                        // Find the parent type's declaration
                        if let Some((parent_uri, parent_line, parent_line_text, parent_kind)) =
                            self.find_type_declaration(&parent_name)
                        {
                            let mut item_json = serde_json::json!({
                                "name": parent_name,
                                "kind": parent_kind,
                                "uri": parent_uri,
                                "range": {
                                    "start": { "line": parent_line, "character": 0 },
                                    "end": { "line": parent_line, "character": parent_line_text.len() }
                                },
                                "selectionRange": {
                                    "start": { "line": parent_line, "character": 0 },
                                    "end": { "line": parent_line, "character": parent_line_text.len() }
                                }
                            });
                            // Add deprecated tag if the parent declaration has @deprecated
                            if parent_line_text.contains("@deprecated") {
                                item_json["tags"] = serde_json::json!([1]);
                            }
                            supertypes.push(item_json);
                        } else {
                            // Parent not found in documents — synthetic entry
                            supertypes.push(serde_json::json!({
                                "name": parent_name,
                                "kind": 5,
                                "uri": doc_uri,
                                "range": {
                                    "start": { "line": line_idx, "character": 0 },
                                    "end": { "line": line_idx, "character": line_text.len() }
                                },
                                "selectionRange": {
                                    "start": { "line": line_idx, "character": 0 },
                                    "end": { "line": line_idx, "character": line_text.len() }
                                }
                            }));
                        }
                    }
                }
            }
        }

        Some(Value::Array(supertypes))
    }

    fn handle_outgoing_calls(&self, params: &Value) -> Option<Value> {
        let item = params.get("item")?;
        let caller_name = item.get("name")?.as_str()?;
        let caller_uri = item.get("uri")?.as_str()?;
        let caller_line =
            usize::try_from(item.get("range")?.get("start")?.get("line")?.as_u64()?).ok()?;

        let content = self.documents.get(caller_uri)?;
        let lines: Vec<&str> = content.lines().collect();

        // Find the end of the function: next function declaration or end of file
        let fn_keywords = ["fn ", "function ", "def ", "method "];
        let body_end = lines
            .iter()
            .enumerate()
            .skip(caller_line + 1)
            .find(|(_, line)| {
                let trimmed = line.trim_start();
                fn_keywords.iter().any(|kw| trimmed.starts_with(kw))
            })
            .map_or(lines.len(), |(idx, _)| idx);

        // Collect all known function names across all documents
        let mut known_functions: Vec<(String, String, usize, usize)> = Vec::new(); // (name, uri, line, line_len)
        for (doc_uri, doc_content) in &self.documents {
            for (line_idx, line_text) in doc_content.lines().enumerate() {
                let trimmed = line_text.trim_start();
                for kw in &fn_keywords {
                    if let Some(after_kw) = trimmed.strip_prefix(kw) {
                        let fn_name: String = after_kw
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if !fn_name.is_empty() && fn_name != caller_name {
                            known_functions.push((
                                fn_name,
                                doc_uri.clone(),
                                line_idx,
                                line_text.len(),
                            ));
                        }
                    }
                }
            }
        }

        let mut calls = Vec::new();
        for (fn_name, fn_uri, fn_line, fn_line_len) in &known_functions {
            let mut from_ranges = Vec::new();

            for line_idx in (caller_line + 1)..body_end {
                if let Some(line_text) = lines.get(line_idx)
                    && line_text.contains(fn_name.as_str())
                {
                    from_ranges.push(serde_json::json!({
                        "start": { "line": line_idx, "character": 0 },
                        "end": { "line": line_idx, "character": line_text.len() }
                    }));
                }
            }

            if !from_ranges.is_empty() {
                let mut to_item = serde_json::json!({
                    "name": fn_name,
                    "kind": 12,
                    "uri": fn_uri,
                    "range": {
                        "start": { "line": fn_line, "character": 0 },
                        "end": { "line": fn_line, "character": fn_line_len }
                    },
                    "selectionRange": {
                        "start": { "line": fn_line, "character": 0 },
                        "end": { "line": fn_line, "character": fn_line_len }
                    }
                });
                // Add deprecated tag if the function declaration has @deprecated
                let target_line = self
                    .documents
                    .get(fn_uri.as_str())
                    .and_then(|c| c.lines().nth(*fn_line))
                    .unwrap_or("");
                if target_line.contains("@deprecated") {
                    to_item["tags"] = serde_json::json!([1]);
                }
                calls.push(serde_json::json!({
                    "to": to_item,
                    "fromRanges": from_ranges
                }));
            }
        }

        Some(Value::Array(calls))
    }

    /// Find a type declaration by name across all documents.
    /// Returns `(uri, line_idx, line_text, kind)`.
    fn find_type_declaration(&self, name: &str) -> Option<(String, usize, String, u32)> {
        let type_keywords: &[(&str, u32)] = &[
            ("struct ", 23),
            ("class ", 5),
            ("interface ", 11),
            ("trait ", 11),
            ("enum ", 10),
        ];

        for (doc_uri, content) in &self.documents {
            for (line_idx, line_text) in content.lines().enumerate() {
                let trimmed = line_text.trim_start();
                for &(kw, kind) in type_keywords {
                    if let Some(after_kw) = trimmed.strip_prefix(kw) {
                        let decl_name: String = after_kw
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if decl_name == name {
                            return Some((doc_uri.clone(), line_idx, line_text.to_string(), kind));
                        }
                    }
                }
            }
        }

        None
    }

    fn handle_pull_diagnostics(&self, params: &Value) -> Value {
        if !self.args.pull_diagnostics {
            return serde_json::json!({
                "kind": "full",
                "items": []
            });
        }

        let uri = params
            .get("textDocument")
            .and_then(|td| td.get("uri"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        let line_count = self.documents.get(uri).map_or(0, |c| c.lines().count());
        let message = if self.args.report_open_count {
            let n = self.documents.len();
            format!("mockls: mock diagnostic ({line_count} lines, {n} open)")
        } else {
            format!("mockls: mock diagnostic ({line_count} lines)")
        };

        let mut items = vec![serde_json::json!({
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            },
            "severity": 2,
            "source": "mockls",
            "message": message
        })];
        items.extend(parse_extra_diagnostics(&self.args.extra_diagnostic));

        serde_json::json!({
            "kind": "full",
            "items": items
        })
    }

    /// Serves `workspace/diagnostic`: one `full` report per scanned document off
    /// the in-memory project model — no per-file open required.
    ///
    /// A document is dirty (one error diagnostic, plus any `--extra-diagnostic`)
    /// when its content contains the marker `DIRTY`; otherwise it reports clean
    /// (empty `items`). Reporting clean documents too lets a `catenary
    /// diagnostics .` receipt exercise the clean-collapse render rule.
    fn handle_workspace_diagnostics(&self) -> Value {
        let extra = parse_extra_diagnostics(&self.args.extra_diagnostic);
        let mut reports = Vec::new();
        for (uri, content) in &self.documents {
            let mut diagnostics = Vec::new();
            if content.contains("DIRTY") {
                let line_count = content.lines().count();
                diagnostics.push(serde_json::json!({
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "severity": 1,
                    "source": "mockls",
                    "message": format!("mockls: workspace diagnostic ({line_count} lines)")
                }));
                diagnostics.extend(extra.iter().cloned());
            }
            reports.push(serde_json::json!({
                "uri": uri,
                "kind": "full",
                "version": Value::Null,
                "items": diagnostics
            }));
        }
        serde_json::json!({ "items": reports })
    }

    fn handle_code_action(&self, params: &Value) -> Value {
        if self.args.no_code_actions {
            return Value::Array(Vec::new());
        }

        let context = params.get("context");
        let diagnostics = context
            .and_then(|c| c.get("diagnostics"))
            .and_then(Value::as_array);

        let mut actions = Vec::new();

        if let Some(diags) = diagnostics {
            for diag in diags {
                let source = diag.get("source").and_then(Value::as_str).unwrap_or("");
                if source == "mockls" {
                    let message = diag
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    actions.push(serde_json::json!({
                        "title": format!("fix: {message}"),
                        "kind": "quickfix",
                        "diagnostics": [diag]
                    }));

                    if self.args.multi_fix {
                        actions.push(serde_json::json!({
                            "title": format!("fix: alternative for {message}"),
                            "kind": "quickfix",
                            "diagnostics": [diag]
                        }));
                    }
                }
            }
        }

        // Always include a refactor action to verify Catenary filters it out
        actions.push(serde_json::json!({
            "title": "refactor: extract variable",
            "kind": "refactor"
        }));

        Value::Array(actions)
    }

    fn handle_document_symbols(&self, params: &Value) -> Option<Value> {
        let uri = params
            .get("textDocument")
            .and_then(|td| td.get("uri"))
            .and_then(Value::as_str)?;

        let content = self.documents.get(uri)?;
        Some(Value::Array(extract_symbols(content)))
    }

    fn handle_workspace_symbols(&self, params: &Value) -> Value {
        let query = params.get("query").and_then(Value::as_str).unwrap_or("");

        if query.is_empty() && self.args.no_empty_query {
            return Value::Array(Vec::new());
        }

        let mut all_symbols = Vec::new();
        for (uri, content) in &self.documents {
            for mut sym in extract_symbols(content) {
                let matches = sym
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| query.is_empty() || n.contains(query));

                if matches && let Some(range) = sym.get("range").cloned() {
                    if let Some(obj) = sym.as_object_mut() {
                        if self.args.resolve_provider {
                            // URI-only location (no range) — client must resolve
                            obj.insert("location".to_string(), serde_json::json!({ "uri": uri }));
                        } else {
                            obj.insert(
                                "location".to_string(),
                                serde_json::json!({ "uri": uri, "range": range }),
                            );
                        }
                        obj.remove("range");
                        obj.remove("selectionRange");
                    }
                    all_symbols.push(sym);
                }
            }
        }

        Value::Array(all_symbols)
    }

    fn handle_workspace_symbol_resolve(&self, params: &Value) -> Option<Value> {
        let name = params.get("name").and_then(Value::as_str)?;
        let uri = params
            .get("location")
            .and_then(|loc| loc.get("uri"))
            .and_then(Value::as_str)?;

        let content = self.documents.get(uri)?;

        // Find the symbol by name in the document to get its range
        for sym in extract_symbols(content) {
            if sym.get("name").and_then(Value::as_str) == Some(name) {
                let range = sym.get("range")?;
                let mut resolved = params.clone();
                if let Some(obj) = resolved.as_object_mut() {
                    obj.insert(
                        "location".to_string(),
                        serde_json::json!({ "uri": uri, "range": range }),
                    );
                }
                return Some(resolved);
            }
        }

        // Symbol not found — return as-is
        Some(params.clone())
    }

    fn publish_diagnostics(&self, uri: &str) {
        let delay = self.args.diagnostics_delay;
        let uri_owned = uri.to_string();
        let writer = self.writer.clone();
        let publish_version = self.args.publish_version;
        let version = if publish_version {
            Some(self.versions.get(uri).copied().unwrap_or(1))
        } else {
            None
        };
        // Capture line count at publish time so delayed publications
        // reflect the content that triggered them, not later edits.
        let line_count = self.documents.get(uri).map_or(0, |c| c.lines().count());
        let open_count = if self.args.report_open_count {
            Some(self.documents.len())
        } else {
            None
        };
        let extra = parse_extra_diagnostics(&self.args.extra_diagnostic);
        let push_empty = self.args.push_empty;

        if delay > 0 {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                send_diagnostics_notification(
                    &writer, &uri_owned, version, line_count, open_count, &extra, push_empty,
                );
            });
        } else {
            send_diagnostics_notification(
                &self.writer,
                &uri_owned,
                version,
                line_count,
                open_count,
                &extra,
                push_empty,
            );
        }
    }

    fn start_indexing_simulation(&self) {
        let delay = self.args.indexing_delay;
        let writer = self.writer.clone();
        let next_id = self.next_request_id.clone();

        std::thread::spawn(move || {
            let token = "mockls-indexing";

            let req_id = next_id.fetch_add(1, Ordering::SeqCst);
            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": "window/workDoneProgress/create",
                    "params": { "token": token }
                }),
            );

            std::thread::sleep(Duration::from_millis(50));

            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "begin", "title": "Indexing", "percentage": 0 }
                    }
                }),
            );

            std::thread::sleep(Duration::from_millis(delay));

            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "end", "message": "Indexing complete" }
                    }
                }),
            );
        });
    }

    fn simulate_progress_around_diagnostics(&self, uri: &str) {
        let uri_owned = uri.to_string();
        let writer = self.writer.clone();
        let next_id = self.next_request_id.clone();
        let no_diagnostics = self.args.no_push_diagnostics;
        let publish_version = self.args.publish_version;
        let diagnostics_delay = self.args.diagnostics_delay;
        let line_count = self.documents.get(uri).map_or(0, |c| c.lines().count());
        let open_count = if self.args.report_open_count {
            Some(self.documents.len())
        } else {
            None
        };
        let version = if publish_version {
            Some(self.versions.get(uri).copied().unwrap_or(1))
        } else {
            None
        };
        let extra = parse_extra_diagnostics(&self.args.extra_diagnostic);

        std::thread::spawn(move || {
            let token = "mockls-checking";

            let req_id = next_id.fetch_add(1, Ordering::SeqCst);
            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": "window/workDoneProgress/create",
                    "params": { "token": token }
                }),
            );

            std::thread::sleep(Duration::from_millis(50));

            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "begin", "title": "Checking", "percentage": 0 }
                    }
                }),
            );

            if diagnostics_delay > 0 {
                std::thread::sleep(Duration::from_millis(diagnostics_delay));
            } else {
                std::thread::sleep(Duration::from_millis(100));
            }

            if !no_diagnostics {
                send_diagnostics_notification(
                    &writer, &uri_owned, version, line_count, open_count, &extra, false,
                );
            }

            std::thread::sleep(Duration::from_millis(50));

            send_message(
                &writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": {
                        "token": token,
                        "value": { "kind": "end", "message": "Checking complete" }
                    }
                }),
            );
        });
    }

    /// Simulates flycheck: progress Begin → spawn subprocess → wait →
    /// publish diagnostics → progress End. Runs in a background thread
    /// so the main message loop stays responsive.
    fn run_flycheck(&self, uri: &str, command: &str) {
        let uri_owned = uri.to_string();
        let command_owned = command.to_string();
        let writer = self.writer.clone();
        let next_id = self.next_request_id.clone();
        let no_diagnostics = self.args.no_push_diagnostics;
        let publish_version = self.args.publish_version;
        let flycheck_ticks = self.args.flycheck_ticks;
        let no_progress = self.args.flycheck_no_progress;
        let line_count = self.documents.get(uri).map_or(0, |c| c.lines().count());
        let open_count = if self.args.report_open_count {
            Some(self.documents.len())
        } else {
            None
        };
        let version = if publish_version {
            Some(self.versions.get(uri).copied().unwrap_or(1))
        } else {
            None
        };
        let extra = parse_extra_diagnostics(&self.args.extra_diagnostic);

        std::thread::spawn(move || {
            let token = "mockls-flycheck";

            if !no_progress {
                // Create progress token
                let req_id = next_id.fetch_add(1, Ordering::SeqCst);
                send_message(
                    &writer,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "window/workDoneProgress/create",
                        "params": { "token": token }
                    }),
                );

                std::thread::sleep(Duration::from_millis(50));

                // Progress Begin
                send_message(
                    &writer,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "$/progress",
                        "params": {
                            "token": token,
                            "value": { "kind": "begin", "title": "Flycheck", "percentage": 0 }
                        }
                    }),
                );
            }

            // Spawn the flycheck subprocess and wait for it to exit.
            // This is where mockls goes to Sleeping while mockc burns CPU.
            let parts: Vec<&str> = command_owned.split_whitespace().collect();
            if let Some((program, cmd_args)) = parts.split_first() {
                let mut cmd = std::process::Command::new(program);
                cmd.args(cmd_args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                if let Some(ticks) = flycheck_ticks {
                    cmd.arg("--ticks").arg(ticks.to_string());
                }
                let _ = cmd.status();
            }

            // Publish diagnostics after subprocess completes
            if !no_diagnostics {
                send_diagnostics_notification(
                    &writer, &uri_owned, version, line_count, open_count, &extra, false,
                );
            }

            std::thread::sleep(Duration::from_millis(50));

            if !no_progress {
                // Progress End
                send_message(
                    &writer,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "$/progress",
                        "params": {
                            "token": token,
                            "value": { "kind": "end", "message": "Flycheck complete" }
                        }
                    }),
                );
            }
        });
    }

    fn send_register_file_watchers(&self) {
        let mut watcher = serde_json::json!({ "globPattern": &self.args.watcher_glob });
        if let Some(kind) = self.args.watcher_kind {
            watcher["kind"] = serde_json::json!(kind);
        }

        let req_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        send_message(
            &self.writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "client/registerCapability",
                "params": {
                    "registrations": [{
                        "id": "mockls-file-watcher",
                        "method": "workspace/didChangeWatchedFiles",
                        "registerOptions": {
                            "watchers": [watcher]
                        }
                    }]
                }
            }),
        );
    }

    fn send_configuration_request(&self) {
        let req_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        send_message(
            &self.writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "workspace/configuration",
                "params": { "items": [{ "section": "mockls" }] }
            }),
        );
    }

    fn send_response(&mut self, response: &Response) {
        let Ok(json) = serde_json::to_string(response) else {
            return;
        };

        write_framed(&self.writer, &json);

        if self.check_drop_after() {
            std::process::exit(1);
        }
    }

    /// Increment the response counter and return whether `drop_after`
    /// has been reached. Extracted so the counter logic is testable
    /// without triggering `process::exit`.
    fn check_drop_after(&mut self) -> bool {
        self.response_count += 1;
        self.args
            .drop_after
            .is_some_and(|max| self.response_count >= max)
    }
}

/// Extract `(uri, line, col)` from a `textDocument/position` params object.
fn extract_position(params: &Value) -> Option<(&str, usize, usize)> {
    let uri = params
        .get("textDocument")
        .and_then(|td| td.get("uri"))
        .and_then(Value::as_str)?;
    let line = usize::try_from(
        params
            .get("position")
            .and_then(|p| p.get("line"))
            .and_then(Value::as_u64)?,
    )
    .ok()?;
    let col = usize::try_from(
        params
            .get("position")
            .and_then(|p| p.get("character"))
            .and_then(Value::as_u64)?,
    )
    .ok()?;
    Some((uri, line, col))
}

/// Build a JSON `Location` object.
fn location_json(uri: &str, line: usize, start: usize, end: usize) -> Value {
    serde_json::json!({
        "uri": uri,
        "range": {
            "start": { "line": line, "character": start },
            "end": { "line": line, "character": end }
        }
    })
}

/// Write a Content-Length framed JSON string.
fn write_framed(writer: &Writer, json: &str) {
    let header = format!("Content-Length: {}\r\n\r\n", json.len());
    let Ok(mut w) = writer.lock() else { return };
    let _ = w.write_all(header.as_bytes());
    let _ = w.write_all(json.as_bytes());
    let _ = w.flush();
}

/// Send a JSON-RPC message to the client.
fn send_message(writer: &Writer, value: &Value) {
    let Ok(json) = serde_json::to_string(value) else {
        return;
    };
    write_framed(writer, &json);
}

/// Parse `--extra-diagnostic 'source|code|message'` specs into diagnostic
/// JSON objects (severity error). An empty `code` field is omitted from the
/// object. Malformed specs (fewer than two `|`) are skipped.
fn parse_extra_diagnostics(specs: &[String]) -> Vec<Value> {
    specs
        .iter()
        .filter_map(|spec| {
            let mut parts = spec.splitn(3, '|');
            let source = parts.next()?;
            let code = parts.next()?;
            let message = parts.next()?;
            let mut diag = serde_json::json!({
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "severity": 1,
                "source": source,
                "message": message
            });
            if !code.is_empty() {
                diag["code"] = serde_json::json!(code);
            }
            Some(diag)
        })
        .collect()
}

/// Send a `publishDiagnostics` notification.
///
/// `extra` carries additional diagnostics (custom `source`/`code`) merged into
/// the same per-file publish — the multi-source shape rust-analyzer produces
/// (misc 115).
fn send_diagnostics_notification(
    writer: &Writer,
    uri: &str,
    version: Option<i32>,
    line_count: usize,
    open_count: Option<usize>,
    extra: &[Value],
    push_empty: bool,
) {
    // `push_empty` publishes the explicit empty set — a push server's
    // evidence-backed clean (misc 153) — overriding the mock diagnostic and
    // any `--extra-diagnostic`.
    let diagnostics = if push_empty {
        Vec::new()
    } else {
        let message = open_count.map_or_else(
            || format!("mockls: mock diagnostic ({line_count} lines)"),
            |n| format!("mockls: mock diagnostic ({line_count} lines, {n} open)"),
        );
        let mut diagnostics = vec![serde_json::json!({
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            },
            "severity": 2,
            "source": "mockls",
            "message": message
        })];
        diagnostics.extend(extra.iter().cloned());
        diagnostics
    };

    let mut params = serde_json::json!({
        "uri": uri,
        "diagnostics": diagnostics
    });

    if let Some(v) = version {
        params["version"] = serde_json::json!(v);
    }

    send_message(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": params
        }),
    );
}

/// Parse a Content-Length framed message from a buffer.
/// Returns the message string and the number of bytes consumed.
fn try_parse_message(buffer: &[u8]) -> Option<(String, usize)> {
    let header_end = buffer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&buffer[..header_end]).ok()?;

    let mut content_length: Option<usize> = None;
    for line in headers.lines() {
        if line.to_ascii_lowercase().starts_with("content-length:") {
            content_length = line
                .split_once(':')
                .and_then(|(_, v)| v.trim().parse().ok());
        }
    }

    let content_length = content_length?;
    let total = header_end + 4 + content_length;

    if buffer.len() < total {
        return None;
    }

    let body = std::str::from_utf8(&buffer[header_end + 4..total]).ok()?;
    Some((body.to_string(), total))
}

/// Extract the word at a given line and column from content.
fn extract_word(content: &str, line: usize, col: usize) -> Option<String> {
    let line_text = content.lines().nth(line)?;

    if col >= line_text.len() {
        return None;
    }

    let bytes = line_text.as_bytes();

    let start = (0..=col)
        .rev()
        .find(|&i| !is_word_char(bytes[i]))
        .map_or(0, |i| i + 1);

    let end = (col..bytes.len())
        .find(|&i| !is_word_char(bytes[i]))
        .unwrap_or(bytes.len());

    if start >= end {
        return None;
    }

    Some(line_text[start..end].to_string())
}

/// Extract the symbol name from a declaration line. If the position lands on
/// the keyword (e.g., `fn`, `let`), returns the name that follows it.
fn extract_symbol_name(content: &str, line: usize, col: usize) -> Option<String> {
    let word = extract_word(content, line, col)?;

    let keywords = [
        "fn",
        "function",
        "def",
        "let",
        "const",
        "var",
        "struct",
        "class",
        "enum",
        "interface",
        "trait",
        "mod",
        "module",
        "type",
        "method",
        "field",
    ];

    if keywords.contains(&word.as_str()) {
        let line_text = content.lines().nth(line)?;
        let kw_with_space = format!("{word} ");
        let kw_pos = line_text.find(&kw_with_space)?;
        let after_kw = &line_text[kw_pos + kw_with_space.len()..];
        let name: String = after_kw
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() { None } else { Some(name) }
    } else {
        Some(word)
    }
}

/// Find the nearest enclosing function for a given line by searching backwards.
fn find_enclosing_function(content: &str, target_line: usize) -> Option<(String, usize)> {
    let fn_keywords = ["fn ", "function ", "def ", "method "];

    for line_idx in (0..target_line).rev() {
        let line_text = content.lines().nth(line_idx)?;
        let trimmed = line_text.trim_start();

        for kw in &fn_keywords {
            if let Some(after_kw) = trimmed.strip_prefix(kw) {
                let name: String = after_kw
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    return Some((name, line_idx));
                }
            }
        }
    }

    None
}

const fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Extract type annotations from content: maps `symbol_name → type_name`.
///
/// Looks for `: TypeName` after keyword-declared symbols.
/// Example: `let count: Counter = 0` → `("count", "Counter")`.
fn extract_types(content: &str) -> HashMap<String, String> {
    let mut types = HashMap::new();
    let keywords: &[&str] = &[
        "fn ",
        "function ",
        "def ",
        "let ",
        "const ",
        "var ",
        "struct ",
        "class ",
        "enum ",
        "interface ",
        "trait ",
        "mod ",
        "module ",
        "type ",
        "method ",
        "field ",
    ];

    for line_text in content.lines() {
        let trimmed = line_text.trim_start();
        let prefix_len = keywords
            .iter()
            .find_map(|kw| trimmed.starts_with(kw).then_some(kw.len()));

        let Some(prefix_len) = prefix_len else {
            continue;
        };

        let after_keyword = &trimmed[prefix_len..];
        let name: String = after_keyword
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();

        if name.is_empty() {
            continue;
        }

        // Look for `: TypeName` after the name
        let after_name = &after_keyword[name.len()..];
        let Some(colon_pos) = after_name.find(": ") else {
            continue;
        };

        let after_colon = &after_name[colon_pos + 2..];
        let type_name: String = after_colon
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();

        if !type_name.is_empty() {
            types.insert(name, type_name);
        }
    }

    types
}

/// Extract symbol definitions from content.
///
/// Produces hierarchical `DocumentSymbol` responses: definitions that end
/// with `{` open a brace block. Subsequent definitions before the matching
/// `}` become `children` of the opening symbol, and the opener's `range.end`
/// is extended to the closing brace line. Definitions without `{` remain
/// single-line.
fn extract_symbols(content: &str) -> Vec<Value> {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut stack: Vec<(Value, Vec<Value>)> = Vec::new(); // (parent_sym, children)

    for (line_idx, line_text) in lines.iter().enumerate() {
        let trimmed = line_text.trim_start();

        // Handle closing brace: pop the stack and finalize the parent.
        if trimmed.starts_with('}') {
            if let Some((mut parent, children)) = stack.pop() {
                // Extend parent range to this closing brace line.
                if let Some(range) = parent.get_mut("range") {
                    range["end"]["line"] = serde_json::json!(line_idx);
                    range["end"]["character"] = serde_json::json!(line_text.len());
                }
                if !children.is_empty() {
                    parent["children"] = Value::Array(children);
                }
                // Push finalized parent to the outer scope.
                if let Some(outer) = stack.last_mut() {
                    outer.1.push(parent);
                } else {
                    result.push(parent);
                }
            }
            continue;
        }

        let parsed = parse_symbol_line(trimmed);
        let Some((kind_num, prefix_len)) = parsed else {
            continue;
        };

        let after_keyword = &trimmed[prefix_len..];
        let name: String = after_keyword
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();

        if name.is_empty() {
            continue;
        }

        let indent = line_text.len() - trimmed.len();
        let col_start = indent + prefix_len;

        let mut sym = serde_json::json!({
            "name": name,
            "kind": kind_num,
            "range": {
                "start": { "line": line_idx, "character": indent },
                "end": { "line": line_idx, "character": line_text.len() }
            },
            "selectionRange": {
                "start": { "line": line_idx, "character": col_start },
                "end": { "line": line_idx, "character": col_start + name.len() }
            }
        });
        if trimmed.contains("@deprecated") {
            sym["tags"] = serde_json::json!([1]);
        }

        // Check if this line opens a brace block.
        if trimmed.contains('{') && !trimmed.contains('}') {
            stack.push((sym, Vec::new()));
        } else if let Some(outer) = stack.last_mut() {
            outer.1.push(sym);
        } else {
            result.push(sym);
        }
    }

    // Drain any unclosed blocks (malformed input).
    while let Some((mut parent, children)) = stack.pop() {
        if !children.is_empty() {
            parent["children"] = Value::Array(children);
        }
        if let Some(outer) = stack.last_mut() {
            outer.1.push(parent);
        } else {
            result.push(parent);
        }
    }

    result
}

/// Parses a keyword prefix from a trimmed line, returning `(SymbolKind, prefix_len)`.
fn parse_symbol_line(trimmed: &str) -> Option<(u32, usize)> {
    if trimmed.starts_with("fn ") {
        Some((12, 3))
    } else if trimmed.starts_with("function ") {
        Some((12, 9))
    } else if trimmed.starts_with("def ") {
        Some((12, 4))
    } else if trimmed.starts_with("let ") {
        Some((13, 4))
    } else if trimmed.starts_with("const ") {
        Some((14, 6))
    } else if trimmed.starts_with("var ") {
        Some((13, 4))
    } else if trimmed.starts_with("struct ") {
        Some((23, 7))
    } else if trimmed.starts_with("class ") {
        Some((5, 6))
    } else if trimmed.starts_with("enum ") {
        Some((10, 5))
    } else if trimmed.starts_with("interface ") {
        Some((11, 10))
    } else if trimmed.starts_with("trait ") {
        Some((11, 6))
    } else if trimmed.starts_with("mod ") {
        Some((2, 4))
    } else if trimmed.starts_with("module ") {
        Some((2, 7))
    } else if trimmed.starts_with("type ") {
        Some((26, 5))
    } else if trimmed.starts_with("method ") {
        Some((6, 7))
    } else if trimmed.starts_with("field ") {
        Some((8, 6))
    } else {
        None
    }
}

fn main() {
    let args = Args::parse();
    let writer = stdout_writer();
    let mut server = MockServer::new(args, writer);
    let mut stdin = std::io::stdin().lock();
    server.run(&mut stdin);
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const MOCK_LANG_A: &str = "yX4Za";

    fn default_args() -> Args {
        Args {
            name: MOCK_LANG_A.to_string(),
            workspace_folders: false,
            indexing_delay: 0,
            response_delay: 0,
            diagnostics_delay: 0,
            no_push_diagnostics: false,
            push_empty: false,
            pull_diagnostics: false,
            workspace_diagnostics: false,
            fail_pull: false,
            diagnostics_on_save: false,
            drop_after: None,
            hang_on: vec![],
            fail_on: vec![],
            die_on: vec![],
            die_once_file: None,
            send_configuration_request: false,
            publish_version: false,
            progress_on_change: false,
            cpu_busy: None,
            flycheck_command: None,
            advertise_save: false,
            notification_log: None,
            log_pid_suffix: false,
            request_log: None,
            content_modified_once: false,
            cpu_on_workspace_change: None,
            cpu_on_initialized: None,
            log_init_params: None,
            flycheck_ticks: None,
            flycheck_no_progress: false,
            scan_roots: false,
            no_code_actions: false,
            no_rename: false,
            no_type_hierarchy: false,
            multi_fix: false,
            resolve_provider: false,
            no_empty_query: false,
            register_file_watchers: false,
            watcher_glob: "**/*".to_string(),
            watcher_kind: None,
            report_open_count: false,
            reject_null_workspace: false,
            stderr_message: None,
            stderr_length: None,
            report_env: None,
            extra_diagnostic: vec![],
        }
    }

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    fn extract_messages(data: &[u8]) -> Vec<Value> {
        let mut messages = Vec::new();
        let mut buf = data.to_vec();
        while let Some((msg, consumed)) = try_parse_message(&buf) {
            if let Ok(v) = serde_json::from_str::<Value>(&msg) {
                messages.push(v);
            }
            buf.drain(..consumed);
        }
        messages
    }

    fn run_server_with(args: Args, input: &[u8]) -> Vec<Value> {
        let (writer, buf) = buffer_writer();
        let mut server = MockServer::new(args, writer);
        let mut reader = Cursor::new(input.to_vec());
        server.run(&mut reader);
        let data = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        extract_messages(&data)
    }

    fn run_server_wait(args: Args, input: &[u8], wait_ms: u64) -> Vec<Value> {
        let (writer, buf) = buffer_writer();
        let mut server = MockServer::new(args, writer);
        let mut reader = Cursor::new(input.to_vec());
        server.run(&mut reader);
        std::thread::sleep(Duration::from_millis(wait_ms));
        let data = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        extract_messages(&data)
    }

    fn initialize_request(id: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": "file:///tmp/test"
            }
        })
        .to_string()
    }

    fn shutdown_request(id: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "shutdown",
            "params": null
        })
        .to_string()
    }

    fn did_open_notification(uri: &str, text: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "mock",
                    "version": 1,
                    "text": text
                }
            }
        })
        .to_string()
    }

    fn hover_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    fn definition_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    #[test]
    fn test_initialize_response_valid() {
        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(default_args(), &input);

        assert!(!messages.is_empty(), "Expected at least one response");
        let resp = &messages[0];
        assert_eq!(resp["id"], 1);
        assert!(resp["result"].is_object(), "Expected result object");
        assert!(
            resp["result"]["capabilities"].is_object(),
            "Expected capabilities"
        );
        assert!(resp["error"].is_null(), "Expected no error");

        let caps = &resp["result"]["capabilities"];
        assert_eq!(caps["hoverProvider"], true);
        assert_eq!(caps["definitionProvider"], true);
        assert_eq!(caps["referencesProvider"], true);
        assert_eq!(caps["documentSymbolProvider"], true);
        assert_eq!(
            caps["typeHierarchyProvider"], true,
            "typeHierarchyProvider should be present by default"
        );
        assert!(
            caps["renameProvider"].is_object(),
            "renameProvider should be present by default"
        );
        assert_eq!(caps["renameProvider"]["prepareProvider"], true);
        assert_eq!(
            caps["callHierarchyProvider"], true,
            "callHierarchyProvider should be present by default"
        );
        assert_eq!(
            caps["codeActionProvider"], true,
            "codeActionProvider should be present by default"
        );
    }

    #[test]
    fn test_initialize_workspace_folders_capability() {
        let mut args = default_args();
        args.workspace_folders = true;

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);
        let ws = &messages[0]["result"]["capabilities"]["workspace"]["workspaceFolders"];
        assert_eq!(ws["supported"], true);
        assert_eq!(ws["changeNotifications"], true);
    }

    #[test]
    fn test_hover_response_structure() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello()\necho hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover on 'echo' (regular word)
        input.extend(frame(&hover_request(2, uri, 1, 0)));
        // Hover on 'fn' keyword at (0,0) — should resolve to 'hello'
        input.extend(frame(&hover_request(3, uri, 0, 0)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(default_args(), &input);

        let hover_echo = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        assert!(hover_echo["error"].is_null(), "Expected no error");
        let result = &hover_echo["result"];
        assert!(result.is_object());
        assert_eq!(result["contents"]["kind"], "markdown");
        let value = result["contents"]["value"].as_str().unwrap_or("");
        assert!(value.contains("echo"), "Expected 'echo' in hover content");

        // Hover on keyword should return symbol name, not 'fn'
        let hover_kw = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("hover response with id=3");

        assert!(hover_kw["error"].is_null(), "Expected no error");
        let kw_value = hover_kw["result"]["contents"]["value"]
            .as_str()
            .unwrap_or("");
        assert!(
            kw_value.contains("hello"),
            "Hover on keyword should contain 'hello', got: {kw_value}"
        );
    }

    #[test]
    fn test_definition_response_structure() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn my_func() {}\nmy_func\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&definition_request(2, uri, 1, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response with id=2");

        assert!(def["error"].is_null(), "Expected no error");
        let result = &def["result"];
        assert_eq!(result["uri"], uri);
        assert_eq!(result["range"]["start"]["line"], 0);
        assert_eq!(result["range"]["start"]["character"], 0);
        assert_eq!(
            result["range"]["end"]["character"], 10,
            "end = col_idx + \"fn my_func\".len()"
        );
    }

    #[test]
    fn test_diagnostics_notification_structure() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "#!/bin/bash\necho hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(default_args(), &input);

        let diag = messages
            .iter()
            .find(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .expect("publishDiagnostics notification");

        let params = &diag["params"];
        assert_eq!(params["uri"], uri);
        let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
        assert!(!diagnostics.is_empty());

        let d = &diagnostics[0];
        assert_eq!(d["severity"], 2);
        assert_eq!(d["source"], "mockls");
        assert!(
            d["message"]
                .as_str()
                .unwrap_or("")
                .contains("mock diagnostic")
        );
    }

    #[test]
    fn test_progress_sequence() {
        let mut args = default_args();
        args.indexing_delay = 100;

        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })
        .to_string();

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&initialized));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_wait(args, &input, 250);

        let has_create = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("window/workDoneProgress/create")
        });
        assert!(
            has_create,
            "Expected workDoneProgress/create. Got: {messages:?}"
        );

        let has_begin = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("$/progress")
                && m["params"]["value"]["kind"] == "begin"
        });
        assert!(has_begin, "Expected $/progress begin. Got: {messages:?}");

        let has_end = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("$/progress")
                && m["params"]["value"]["kind"] == "end"
        });
        assert!(has_end, "Expected $/progress end. Got: {messages:?}");
    }

    #[test]
    fn test_content_length_framing() {
        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let (writer, buf) = buffer_writer();
        let mut server = MockServer::new(default_args(), writer);
        let mut reader = Cursor::new(input);
        server.run(&mut reader);

        let output_str = {
            let data = buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            String::from_utf8_lossy(&data).into_owned()
        };
        let mut remaining = output_str.as_str();

        let mut count = 0;
        while !remaining.is_empty() {
            let header_end = remaining.find("\r\n\r\n").expect("Content-Length header");
            let headers = &remaining[..header_end];

            let cl_line = headers
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .expect("Content-Length header line");

            let cl: usize = cl_line
                .split_once(':')
                .expect("colon in header")
                .1
                .trim()
                .parse()
                .expect("valid content-length");

            let body_start = header_end + 4;
            let body = &remaining[body_start..body_start + cl];

            let _: Value = serde_json::from_str(body).expect("valid JSON body");

            remaining = &remaining[body_start + cl..];
            count += 1;
        }

        assert!(count >= 2, "Expected at least 2 framed messages");
    }

    #[test]
    fn test_request_id_echo() {
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "initialize",
            "params": { "processId": null, "capabilities": {}, "rootUri": null }
        })
        .to_string();
        let shutdown = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "string-id",
            "method": "shutdown",
            "params": null
        })
        .to_string();

        let mut input = frame(&init);
        input.extend(frame(&shutdown));

        let messages = run_server_with(default_args(), &input);

        assert_eq!(messages[0]["id"], 42, "Init should echo numeric id");

        let shutdown_resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some("string-id"));
        assert!(shutdown_resp.is_some(), "Shutdown should echo string id");
    }

    fn type_definition_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/typeDefinition",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    #[test]
    fn test_extract_symbols_all_kinds() {
        let content = "\
struct MyStruct
class MyClass
enum MyEnum
interface MyInterface
trait MyTrait
mod my_mod
module my_module
type MyType
method my_method
field my_field
fn my_func
let my_var
const MY_CONST
";
        let symbols = extract_symbols(content);
        let kinds: Vec<(String, u64)> = symbols
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().expect("name").to_string(),
                    s["kind"].as_u64().expect("kind"),
                )
            })
            .collect();

        assert!(
            kinds.contains(&("MyStruct".to_string(), 23)),
            "struct → Struct(23)"
        );
        assert!(
            kinds.contains(&("MyClass".to_string(), 5)),
            "class → Class(5)"
        );
        assert!(
            kinds.contains(&("MyEnum".to_string(), 10)),
            "enum → Enum(10)"
        );
        assert!(
            kinds.contains(&("MyInterface".to_string(), 11)),
            "interface → Interface(11)"
        );
        assert!(
            kinds.contains(&("MyTrait".to_string(), 11)),
            "trait → Interface(11)"
        );
        assert!(
            kinds.contains(&("my_mod".to_string(), 2)),
            "mod → Module(2)"
        );
        assert!(
            kinds.contains(&("my_module".to_string(), 2)),
            "module → Module(2)"
        );
        assert!(
            kinds.contains(&("MyType".to_string(), 26)),
            "type → TypeParameter(26)"
        );
        assert!(
            kinds.contains(&("my_method".to_string(), 6)),
            "method → Method(6)"
        );
        assert!(
            kinds.contains(&("my_field".to_string(), 8)),
            "field → Field(8)"
        );
        assert!(
            kinds.contains(&("my_func".to_string(), 12)),
            "fn → Function(12)"
        );
        assert!(
            kinds.contains(&("my_var".to_string(), 13)),
            "let → Variable(13)"
        );
        assert!(
            kinds.contains(&("MY_CONST".to_string(), 14)),
            "const → Constant(14)"
        );
    }

    #[test]
    fn test_type_annotations_parsed() {
        let content = "\
let x: Foo
fn bar: Result
const PI: f64
";
        let types = extract_types(content);
        assert_eq!(types.get("x").map(String::as_str), Some("Foo"));
        assert_eq!(types.get("bar").map(String::as_str), Some("Result"));
        assert_eq!(types.get("PI").map(String::as_str), Some("f64"));
    }

    #[test]
    fn test_type_definition_cross_file() {
        let uri_a = "file:///tmp/types.yX4Za";
        let text_a = "struct Foo\n";
        let uri_b = "file:///tmp/usage.yX4Za";
        let text_b = "let x: Foo\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // Request typeDefinition on 'x' in uri_b (line 0, character 4)
        input.extend(frame(&type_definition_request(2, uri_b, 0, 4)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let td = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("typeDefinition response with id=2");

        assert!(td["error"].is_null(), "Expected no error");
        let result = &td["result"];
        assert_eq!(
            result["uri"], uri_a,
            "Type definition should point to the file with struct Foo"
        );
        assert_eq!(result["range"]["start"]["line"], 0);
        assert_eq!(result["range"]["start"]["character"], 0);
        assert_eq!(
            result["range"]["end"]["character"], 10,
            "end = col_idx + \"struct Foo\".len()"
        );
    }

    #[test]
    fn test_definition_cross_file() {
        let uri_a = "file:///tmp/defs.yX4Za";
        let text_a = "fn helper()\n";
        let uri_b = "file:///tmp/caller.yX4Za";
        let text_b = "helper\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // Request definition on 'helper' in uri_b (line 0, character 0)
        input.extend(frame(&definition_request(2, uri_b, 0, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response with id=2");

        assert!(def["error"].is_null(), "Expected no error");
        let result = &def["result"];
        assert_eq!(
            result["uri"], uri_a,
            "Definition should point to the file with fn helper()"
        );
        assert_eq!(result["range"]["start"]["line"], 0);
        assert_eq!(result["range"]["start"]["character"], 0);
        assert_eq!(
            result["range"]["end"]["character"], 9,
            "end = col_idx + \"fn helper\".len()"
        );
    }

    #[test]
    fn test_hover_on_keyword_returns_symbol_name() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn callee()\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover at (0, 0) — lands on the 'fn' keyword
        input.extend(frame(&hover_request(2, uri, 0, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            value.contains("callee"),
            "Hover on keyword should return 'callee', got: {value}"
        );
        assert!(
            !value.contains("```\nfn\n```"),
            "Hover should not be bare keyword 'fn', got: {value}"
        );
    }

    #[test]
    fn test_hover_on_symbol_name_returns_name() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn callee()\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover at (0, 3) — lands on the 'c' in 'callee'
        input.extend(frame(&hover_request(2, uri, 0, 3)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            value.contains("callee"),
            "Hover on symbol name should return 'callee', got: {value}"
        );
    }

    #[test]
    fn test_hover_on_struct_keyword() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "struct MyStruct\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover at (0, 0) — lands on the 'struct' keyword
        input.extend(frame(&hover_request(2, uri, 0, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response with id=2");

        let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            value.contains("MyStruct"),
            "Hover on struct keyword should return 'MyStruct', got: {value}"
        );
        assert!(
            !value.contains("```\nstruct\n```"),
            "Hover should not be bare keyword 'struct', got: {value}"
        );
    }

    #[test]
    fn test_definition_with_imports() {
        let uri_defs = "file:///tmp/defs.yX4Za";
        let text_defs = "fn helper()\n";
        let uri_a = "file:///tmp/a.yX4Za";
        let text_a = "from defs import helper\nhelper\n";
        let uri_b = "file:///tmp/b.yX4Za";
        let text_b = "helper\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_defs, text_defs)));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // Definition on 'helper' in a.sh (line 1, col 0) — import should resolve to defs.sh
        input.extend(frame(&definition_request(2, uri_a, 1, 0)));
        // Definition on 'helper' in b.sh (line 0, col 0) — no import, cross-file fallback
        input.extend(frame(&definition_request(3, uri_b, 0, 0)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(default_args(), &input);

        // a.sh: import resolves to defs.sh
        let def_a = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response with id=2");
        assert!(def_a["error"].is_null(), "Expected no error for a.yX4Za");
        assert_eq!(
            def_a["result"]["uri"], uri_defs,
            "Import in a.yX4Za should resolve to defs.yX4Za"
        );
        assert_eq!(def_a["result"]["range"]["start"]["character"], 0);
        assert_eq!(
            def_a["result"]["range"]["end"]["character"], 9,
            "end = col_idx + \"fn helper\".len()"
        );

        // b.yX4Za: cross-file fallback also resolves to defs.yX4Za
        let def_b = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("definition response with id=3");
        assert!(def_b["error"].is_null(), "Expected no error for b.yX4Za");
        assert_eq!(
            def_b["result"]["uri"], uri_defs,
            "Fallback in b.yX4Za should resolve to defs.yX4Za"
        );
        assert_eq!(def_b["result"]["range"]["start"]["character"], 0);
        assert_eq!(
            def_b["result"]["range"]["end"]["character"], 9,
            "end = col_idx + \"fn helper\".len()"
        );
    }

    fn prepare_type_hierarchy_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/prepareTypeHierarchy",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    fn supertypes_request(id: u64, item: &Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "typeHierarchy/supertypes",
            "params": { "item": item }
        })
        .to_string()
    }

    fn subtypes_request(id: u64, item: &Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "typeHierarchy/subtypes",
            "params": { "item": item }
        })
        .to_string()
    }

    fn prepare_call_hierarchy_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/prepareCallHierarchy",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    fn outgoing_calls_request(id: u64, item: &Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": item }
        })
        .to_string()
    }

    #[test]
    fn test_mockls_supertypes() {
        let uri = "file:///tmp/hierarchy.yX4Za";
        // Trailing " {" after type names kills the `== → !=` mutant
        // on `*c == '_'` in the `take_while` predicates.
        // Line 0: "interface Animal {"  (len=18)
        // Line 1: "class Dog extends Animal {"  (len=26)
        let text = "interface Animal {\nclass Dog extends Animal {\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // prepareTypeHierarchy on 'Dog' (line 1, character 6)
        input.extend(frame(&prepare_type_hierarchy_request(2, uri, 1, 6)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);

        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepareTypeHierarchy response with id=2");
        assert!(prepare["error"].is_null(), "Expected no error");
        let items = prepare["result"].as_array().expect("result array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "Dog");
        assert_eq!(items[0]["kind"], 5); // Class
        assert_eq!(items[0]["uri"], uri);
        assert_eq!(items[0]["range"]["start"]["line"], 1);
        assert_eq!(items[0]["range"]["start"]["character"], 0);
        assert_eq!(items[0]["range"]["end"]["line"], 1);
        assert_eq!(items[0]["range"]["end"]["character"], 26);
        assert_eq!(items[0]["selectionRange"]["start"]["line"], 1);
        assert_eq!(items[0]["selectionRange"]["end"]["character"], 26);

        // Now request supertypes using the prepared item
        let dog_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri, text)));
        input2.extend(frame(&supertypes_request(11, dog_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);

        let supertypes = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("supertypes response with id=11");
        assert!(supertypes["error"].is_null(), "Expected no error");
        let parents = supertypes["result"].as_array().expect("result array");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0]["name"], "Animal");
        assert_eq!(parents[0]["kind"], 11); // Interface
        assert_eq!(parents[0]["uri"], uri);
        assert_eq!(parents[0]["range"]["start"]["line"], 0);
        assert_eq!(parents[0]["range"]["start"]["character"], 0);
        assert_eq!(parents[0]["range"]["end"]["line"], 0);
        assert_eq!(parents[0]["range"]["end"]["character"], 18);
        assert_eq!(parents[0]["selectionRange"]["start"]["line"], 0);
        assert_eq!(parents[0]["selectionRange"]["end"]["character"], 18);
    }

    #[test]
    fn test_mockls_subtypes() {
        let uri = "file:///tmp/hierarchy.yX4Za";
        // Trailing " {" after parent names forces the `take_while`
        // predicate to stop at ' ', killing the `== → !=` mutant
        // on `*c == '_'` (which would otherwise over-collect).
        // Line 0: "interface Animal"              (len=16)
        // Line 1: "struct Dog extends Animal {"    (len=27)
        // Line 2: "class Cat implements Animal {"  (len=29)
        // Line 3: "interface Vehicle"              (len=17)
        // Line 4: "class Car extends Vehicle {"    (len=27)
        let text = "interface Animal\nstruct Dog extends Animal {\nclass Cat implements Animal {\ninterface Vehicle\nclass Car extends Vehicle {\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // prepareTypeHierarchy on 'Animal' (line 0, character 10)
        input.extend(frame(&prepare_type_hierarchy_request(2, uri, 0, 10)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);

        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepareTypeHierarchy response with id=2");
        assert!(prepare["error"].is_null(), "Expected no error");
        let items = prepare["result"].as_array().expect("result array");
        assert_eq!(items[0]["name"], "Animal");
        assert_eq!(items[0]["kind"], 11); // Interface

        // Now request subtypes
        let animal_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri, text)));
        input2.extend(frame(&subtypes_request(11, animal_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);

        let subtypes = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("subtypes response with id=11");
        assert!(subtypes["error"].is_null(), "Expected no error");
        let children = subtypes["result"].as_array().expect("result array");
        assert_eq!(children.len(), 2, "Expected exactly 2 subtypes of Animal");

        let dog = children
            .iter()
            .find(|c| c["name"] == "Dog")
            .expect("Dog in subtypes");
        assert_eq!(dog["kind"], 23, "Dog is a struct (kind 23)");
        assert_eq!(dog["uri"], uri);
        assert_eq!(dog["range"]["start"]["line"], 1);
        assert_eq!(dog["range"]["start"]["character"], 0);
        assert_eq!(dog["range"]["end"]["character"], 27);
        assert_eq!(dog["selectionRange"]["start"]["line"], 1);
        assert_eq!(dog["selectionRange"]["end"]["character"], 27);

        let cat = children
            .iter()
            .find(|c| c["name"] == "Cat")
            .expect("Cat in subtypes");
        assert_eq!(cat["kind"], 5, "Cat is a class (kind 5)");
        assert_eq!(cat["uri"], uri);
        assert_eq!(cat["range"]["start"]["line"], 2);
        assert_eq!(cat["range"]["end"]["character"], 29);

        // Car extends Vehicle, not Animal — must not appear
        assert!(
            !children
                .iter()
                .filter_map(|c| c["name"].as_str())
                .any(|n| n == "Car"),
            "Car should not be a subtype of Animal"
        );
    }

    #[test]
    fn test_mockls_outgoing_calls() {
        let uri = "file:///tmp/calls.yX4Za";
        // Line 0: "fn helper()"  (len=11)
        // Line 1: "fn caller()"  (len=11)
        // Line 2: "    helper"   (len=10)
        let text = "fn helper()\nfn caller()\n    helper\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // prepareCallHierarchy on 'caller' (line 1, character 3)
        input.extend(frame(&prepare_call_hierarchy_request(2, uri, 1, 3)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);

        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepareCallHierarchy response with id=2");
        assert!(prepare["error"].is_null(), "Expected no error");
        let items = prepare["result"].as_array().expect("result array");
        assert_eq!(items[0]["name"], "caller");

        // Now request outgoing calls
        let caller_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri, text)));
        input2.extend(frame(&outgoing_calls_request(11, caller_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);

        let outgoing = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("outgoingCalls response with id=11");
        assert!(outgoing["error"].is_null(), "Expected no error");
        let calls = outgoing["result"].as_array().expect("result array");
        assert_eq!(calls.len(), 1, "Expected 1 outgoing call");
        assert_eq!(calls[0]["to"]["name"], "helper");
        assert_eq!(calls[0]["to"]["kind"], 12); // Function
        assert_eq!(calls[0]["to"]["uri"], uri);
        assert_eq!(
            calls[0]["to"]["range"]["start"]["line"], 0,
            "helper is declared on line 0"
        );
        assert_eq!(calls[0]["to"]["range"]["start"]["character"], 0);
        assert_eq!(
            calls[0]["to"]["range"]["end"]["character"], 11,
            "end = len(\"fn helper()\")"
        );
        assert_eq!(calls[0]["to"]["selectionRange"]["start"]["line"], 0);
        assert_eq!(calls[0]["to"]["selectionRange"]["end"]["character"], 11);

        let from_ranges = calls[0]["fromRanges"].as_array().expect("fromRanges");
        assert_eq!(from_ranges.len(), 1, "Expected exactly one fromRange");
        assert_eq!(from_ranges[0]["start"]["line"], 2);
        assert_eq!(from_ranges[0]["start"]["character"], 0);
        assert_eq!(
            from_ranges[0]["end"]["character"], 10,
            "end = len(\"    helper\")"
        );
    }

    #[test]
    fn test_mockls_deprecated_tag() {
        let uri = "file:///tmp/deprecated.yX4Za";
        let text = "fn old_func() @deprecated\nstruct OldType @deprecated\nfn normal()\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Document symbols should include deprecated tags
        let doc_symbols_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/documentSymbol",
            "params": { "textDocument": { "uri": uri } }
        })
        .to_string();
        input.extend(frame(&doc_symbols_req));
        // prepareCallHierarchy on deprecated function (line 0, col 3)
        input.extend(frame(&prepare_call_hierarchy_request(3, uri, 0, 3)));
        // prepareTypeHierarchy on deprecated struct (line 1, col 7)
        input.extend(frame(&prepare_type_hierarchy_request(4, uri, 1, 7)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);

        // Document symbols: deprecated symbols have tags
        let symbols = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("documentSymbol response with id=2");
        let syms = symbols["result"].as_array().expect("result array");

        let old_func = syms
            .iter()
            .find(|s| s["name"] == "old_func")
            .expect("old_func");
        assert_eq!(
            old_func["tags"],
            serde_json::json!([1]),
            "old_func should have DEPRECATED tag"
        );

        let old_type = syms
            .iter()
            .find(|s| s["name"] == "OldType")
            .expect("OldType");
        assert_eq!(
            old_type["tags"],
            serde_json::json!([1]),
            "OldType should have DEPRECATED tag"
        );

        let normal = syms.iter().find(|s| s["name"] == "normal").expect("normal");
        assert!(
            normal.get("tags").is_none() || normal["tags"].is_null(),
            "normal should not have DEPRECATED tag"
        );

        // CallHierarchyItem for deprecated function has tag
        let call_prep = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("prepareCallHierarchy response with id=3");
        let call_items = call_prep["result"].as_array().expect("result array");
        assert_eq!(
            call_items[0]["tags"],
            serde_json::json!([1]),
            "CallHierarchyItem should have DEPRECATED tag"
        );

        // TypeHierarchyItem for deprecated struct has tag
        let type_prep = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(4))
            .expect("prepareTypeHierarchy response with id=4");
        let type_items = type_prep["result"].as_array().expect("result array");
        assert_eq!(
            type_items[0]["tags"],
            serde_json::json!([1]),
            "TypeHierarchyItem should have DEPRECATED tag"
        );
    }

    // ── Request builder helpers ──────────────────────────────────────

    fn references_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/references",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": true }
            }
        })
        .to_string()
    }

    fn implementation_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/implementation",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    fn prepare_rename_request(id: u64, uri: &str, line: u64, character: u64) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/prepareRename",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }
        })
        .to_string()
    }

    fn incoming_calls_request(id: u64, item: &Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "callHierarchy/incomingCalls",
            "params": { "item": item }
        })
        .to_string()
    }

    fn code_action_request(id: u64, uri: &str, diagnostics: &[Value]) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "context": { "diagnostics": diagnostics }
            }
        })
        .to_string()
    }

    fn pull_diagnostics_request(id: u64, uri: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/diagnostic",
            "params": {
                "textDocument": { "uri": uri }
            }
        })
        .to_string()
    }

    fn workspace_symbol_resolve_request(id: u64, item: &Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "workspaceSymbol/resolve",
            "params": item
        })
        .to_string()
    }

    // ── New tests: untested handler functions ────────────────────────

    #[test]
    fn test_prepare_rename_response() {
        let uri = "file:///tmp/rename.yX4Za";
        // "fn my_func" — 'fn' at col 0-1, space at 2, 'my_func' at col 3-9
        let text = "fn my_func\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // prepareRename on 'my_func' at (0, 5)
        input.extend(frame(&prepare_rename_request(2, uri, 0, 5)));
        // prepareRename on 'fn' keyword at (0, 0) — should return null
        input.extend(frame(&prepare_rename_request(3, uri, 0, 0)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(default_args(), &input);

        // Symbol rename: should return range and placeholder
        let rename_sym = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepareRename response with id=2");
        assert!(rename_sym["error"].is_null(), "Expected no error");
        let result = &rename_sym["result"];
        assert!(result.is_object(), "Expected result object for symbol");
        assert_eq!(result["placeholder"], "my_func");
        assert_eq!(result["range"]["start"]["line"], 0);
        assert_eq!(result["range"]["start"]["character"], 3);
        assert_eq!(result["range"]["end"]["line"], 0);
        assert_eq!(result["range"]["end"]["character"], 10);

        // Keyword rename: should return null result
        let rename_kw = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("prepareRename response with id=3");
        assert!(
            rename_kw["error"].is_null(),
            "Expected no error for keyword"
        );
        assert!(
            rename_kw["result"].is_null(),
            "Keyword 'fn' should return null result"
        );
    }

    #[test]
    fn test_references_response() {
        let uri_a = "file:///tmp/defs.yX4Za";
        let text_a = "fn target()\n";
        let uri_b = "file:///tmp/usage.yX4Za";
        // "target" appears at col 0 on line 0 and col 5 on line 1
        let text_b = "target\ncall target here\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // References on 'target' in uri_b, line 0
        input.extend(frame(&references_request(2, uri_b, 0, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let refs = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("references response with id=2");
        assert!(refs["error"].is_null(), "Expected no error");
        let locations = refs["result"].as_array().expect("result array");

        // Should find "target" in multiple places across both files
        assert!(
            locations.len() >= 3,
            "Expected at least 3 references (1 in defs, 2 in usage), got {}",
            locations.len()
        );

        // Verify character positions are plausible (not zero from Default)
        // uri_a line 0: "fn target()" — "target" at col 3
        let a_refs: Vec<&Value> = locations.iter().filter(|l| l["uri"] == uri_a).collect();
        assert!(!a_refs.is_empty(), "Should have references in defs file");
        let a_ref = &a_refs[0];
        assert_eq!(a_ref["range"]["start"]["character"], 3);
        assert_eq!(
            a_ref["range"]["end"]["character"], 9,
            "end = 3 + \"target\".len()"
        );

        // uri_b line 1: "call target here" — "target" at col 5
        let b_line1_refs: Vec<&Value> = locations
            .iter()
            .filter(|l| l["uri"] == uri_b && l["range"]["start"]["line"] == 1)
            .collect();
        assert!(
            !b_line1_refs.is_empty(),
            "Should have reference on line 1 of usage"
        );
        assert_eq!(b_line1_refs[0]["range"]["start"]["character"], 5);
        assert_eq!(
            b_line1_refs[0]["range"]["end"]["character"], 11,
            "end = 5 + \"target\".len()"
        );
    }

    #[test]
    fn test_incoming_calls_response() {
        let uri = "file:///tmp/calls.yX4Za";
        // callee defined on line 0, caller defined on line 1, caller calls callee on line 2
        let text = "fn callee()\nfn caller()\n    callee\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // prepareCallHierarchy on 'callee' (line 0, character 3)
        input.extend(frame(&prepare_call_hierarchy_request(2, uri, 0, 3)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);

        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepareCallHierarchy response");
        let items = prepare["result"].as_array().expect("result array");
        assert_eq!(items[0]["name"], "callee");

        // Now request incoming calls
        let callee_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri, text)));
        input2.extend(frame(&incoming_calls_request(11, callee_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);

        let incoming = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("incomingCalls response with id=11");
        assert!(incoming["error"].is_null(), "Expected no error");
        let calls = incoming["result"].as_array().expect("result array");
        assert_eq!(calls.len(), 1, "Expected 1 incoming call from caller");
        assert_eq!(calls[0]["from"]["name"], "caller");
        assert_eq!(calls[0]["from"]["kind"], 12); // Function

        let from_ranges = calls[0]["fromRanges"].as_array().expect("fromRanges");
        assert!(!from_ranges.is_empty());
        assert_eq!(from_ranges[0]["start"]["line"], 2, "Call site is on line 2");
    }

    #[test]
    fn test_implementation_response() {
        let uri = "file:///tmp/shapes.yX4Za";
        // Shape declared on line 0; Circle/Square implement it; Triangle only
        // extends it — a subtype, but not an implementor.
        let text = "interface Shape\n\
                    struct Circle implements Shape\n\
                    struct Square implements Shape\n\
                    struct Triangle extends Shape\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // implementation on 'Shape' (line 0, character 10 — the name after
        // `interface `).
        input.extend(frame(&implementation_request(2, uri, 0, 10)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let impls = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("implementation response with id=2");
        assert!(impls["error"].is_null(), "Expected no error");
        let locations = impls["result"].as_array().expect("result array");

        // Only the two `implements Shape` types are returned; the `extends`-only
        // Triangle is excluded, proving this is not the subtypes path.
        assert_eq!(
            locations.len(),
            2,
            "Expected exactly 2 implementors (Circle, Square), got {}",
            locations.len()
        );

        let impl_lines: Vec<u64> = locations
            .iter()
            .filter_map(|l| l["range"]["start"]["line"].as_u64())
            .collect();
        assert!(
            impl_lines.contains(&1),
            "Circle (line 1) implements Shape and should be reported"
        );
        assert!(
            impl_lines.contains(&2),
            "Square (line 2) implements Shape and should be reported"
        );
        assert!(
            !impl_lines.contains(&3),
            "Triangle (line 3) only extends Shape and must not be reported"
        );

        // Each location points at the implementing type's name (after `struct `).
        for loc in locations {
            assert_eq!(loc["uri"], uri);
            assert_eq!(
                loc["range"]["start"]["character"], 7,
                "name starts after `struct `"
            );
        }
    }

    // ── New tests: match arm dispatch coverage ──────────────────────

    #[test]
    fn test_code_action_response() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&code_action_request(2, uri, &[])));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let action = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("codeAction response");
        assert!(action["error"].is_null(), "Expected no error");
        let result = action["result"].as_array().expect("result array");
        assert!(
            result.iter().any(|a| a["kind"] == "refactor"),
            "Should include refactor action"
        );
    }

    #[test]
    fn test_pull_diagnostics_response() {
        let mut args = default_args();
        args.pull_diagnostics = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&pull_diagnostics_request(2, uri)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(args, &input);

        let diag = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("diagnostic response");
        assert!(diag["error"].is_null(), "Expected no error");
        let result = &diag["result"];
        assert_eq!(result["kind"], "full");
        let items = result["items"].as_array().expect("items array");
        assert!(!items.is_empty(), "Expected at least one diagnostic item");
        assert_eq!(items[0]["source"], "mockls");
    }

    #[test]
    fn test_pull_diagnostics_fail_pull() {
        let mut args = default_args();
        args.pull_diagnostics = true;
        args.fail_pull = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&pull_diagnostics_request(2, uri)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(args, &input);

        let diag = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("diagnostic response");
        assert!(diag["error"].is_object(), "Should fail with --fail-pull");
        assert_eq!(diag["error"]["code"], -32603);
    }

    #[test]
    fn test_workspace_symbol_resolve_response() {
        let mut args = default_args();
        args.resolve_provider = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn my_func\n";

        // Construct an unresolved workspace symbol item (URI only, no range)
        let unresolved_item = serde_json::json!({
            "name": "my_func",
            "kind": 12,
            "location": { "uri": uri }
        });

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&workspace_symbol_resolve_request(
            2,
            &unresolved_item,
        )));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(args, &input);

        let resolved = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("resolve response");
        assert!(resolved["error"].is_null(), "Expected no error");
        let result = &resolved["result"];
        assert!(
            result["location"]["range"].is_object(),
            "Resolved symbol should have location with range"
        );
        assert_eq!(result["name"], "my_func");
    }

    // ── New tests: handle_request logic ─────────────────────────────

    #[test]
    fn test_fail_on_specific_method() {
        let mut args = default_args();
        args.fail_on = vec!["textDocument/hover".to_string()];
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover should fail
        input.extend(frame(&hover_request(2, uri, 0, 3)));
        // Definition should succeed
        input.extend(frame(&definition_request(3, uri, 0, 3)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(args, &input);

        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response");
        assert!(
            hover["error"].is_object(),
            "Hover should fail when in fail_on"
        );
        assert_eq!(hover["error"]["code"], -32603);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("definition response");
        assert!(
            def["error"].is_null(),
            "Definition should succeed when not in fail_on"
        );
    }

    #[test]
    fn test_hang_on_specific_method() {
        let mut args = default_args();
        args.hang_on = vec!["textDocument/hover".to_string()];
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&hover_request(2, uri, 0, 3)));
        input.extend(frame(&definition_request(3, uri, 0, 3)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(args, &input);

        // Hover should NOT have a response (hung)
        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2));
        assert!(hover.is_none(), "Hover should not respond when hung");

        // Definition should still work
        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("definition response");
        assert!(
            def["error"].is_null(),
            "Definition should succeed when not in hang_on"
        );
    }

    #[test]
    fn test_content_modified_once_retry() {
        let mut args = default_args();
        args.content_modified_once = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\nhello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // First definition request — should get ContentModified error
        input.extend(frame(&definition_request(2, uri, 1, 0)));
        // Second definition request — should succeed
        input.extend(frame(&definition_request(3, uri, 1, 0)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(args, &input);

        let first = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("first definition response");
        assert!(
            first["error"].is_object(),
            "First definition should return ContentModified error"
        );
        assert_eq!(first["error"]["code"], -32801);

        let second = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("second definition response");
        assert!(
            second["error"].is_null(),
            "Second definition should succeed after ContentModified"
        );
        assert_eq!(second["result"]["uri"], uri);
    }

    #[test]
    fn test_reject_null_workspace_both_null() {
        let mut args = default_args();
        args.reject_null_workspace = true;

        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": null
            }
        })
        .to_string();

        let mut input = frame(&init);
        input.extend(frame(&shutdown_request(2)));
        let messages = run_server_with(args, &input);

        let resp = &messages[0];
        assert!(resp["error"].is_object(), "Should reject null workspace");
        assert_eq!(resp["error"]["code"], -32002);
    }

    /// When rootUri is null but workspaceFolders are present, the server
    /// should accept. This distinguishes `&&` from `||` in the guard.
    #[test]
    fn test_reject_null_workspace_folders_present() {
        let mut args = default_args();
        args.reject_null_workspace = true;

        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": null,
                "workspaceFolders": [{"uri": "file:///tmp/test", "name": "test"}]
            }
        })
        .to_string();

        let mut input = frame(&init);
        input.extend(frame(&shutdown_request(2)));
        let messages = run_server_with(args, &input);

        let resp = &messages[0];
        assert!(
            resp["error"].is_null(),
            "Should accept when workspaceFolders are present even if rootUri is null"
        );
    }

    // ── New tests: import resolution and cross-file logic ───────────

    /// When two files define the same symbol, import resolution should
    /// prefer the file matching the import source fragment. This kills
    /// the `delete !` mutant in the import URI check.
    #[test]
    fn test_definition_import_resolution_priority() {
        let uri_defs = "file:///tmp/defs.yX4Za";
        let text_defs = "fn helper()\n";
        let uri_alt = "file:///tmp/alt.yX4Za";
        let text_alt = "fn helper()\n"; // Same definition in different file
        let uri_a = "file:///tmp/a.yX4Za";
        let text_a = "from defs import helper\nhelper\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_defs, text_defs)));
        input.extend(frame(&did_open_notification(uri_alt, text_alt)));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&definition_request(2, uri_a, 1, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response");
        assert!(def["error"].is_null());
        assert_eq!(
            def["result"]["uri"], uri_defs,
            "Import should resolve to defs.yX4Za, not alt.yX4Za"
        );
    }

    // ── New tests: definition first-occurrence fallbacks ────────────

    /// When no def pattern matches anywhere, definition should prefer
    /// a cross-file first-occurrence over the current-doc cursor position.
    #[test]
    fn test_definition_cross_file_first_occurrence_fallback() {
        let uri_a = "file:///tmp/lib.yX4Za";
        let text_a = "greet someone\n"; // "greet" at col 0
        let uri_b = "file:///tmp/main.yX4Za";
        let text_b = "call greet\n"; // cursor here, "greet" at col 5

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_a, text_a)));
        input.extend(frame(&did_open_notification(uri_b, text_b)));
        // Definition on "greet" in uri_b at (0, 5)
        input.extend(frame(&definition_request(2, uri_b, 0, 5)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response");
        assert!(def["error"].is_null());
        assert_eq!(
            def["result"]["uri"], uri_a,
            "Cross-file first-occurrence should be preferred over current-doc"
        );
        assert_eq!(def["result"]["range"]["start"]["character"], 0);
        assert_eq!(
            def["result"]["range"]["end"]["character"], 5,
            "end = 0 + \"greet\".len()"
        );
    }

    /// When no other file contains the word, definition falls back to
    /// the first occurrence in the current document.
    #[test]
    fn test_definition_current_doc_first_occurrence_fallback() {
        let uri = "file:///tmp/test.yX4Za";
        // "unknown" has no def pattern; first occurrence is at line 0, col 5
        let text = "call unknown here\nunknown again\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Definition on "unknown" at (1, 0)
        input.extend(frame(&definition_request(2, uri, 1, 0)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response");
        assert!(def["error"].is_null());
        assert_eq!(def["result"]["uri"], uri);
        assert_eq!(def["result"]["range"]["start"]["line"], 0);
        assert_eq!(def["result"]["range"]["start"]["character"], 5);
        assert_eq!(
            def["result"]["range"]["end"]["character"], 12,
            "end = 5 + \"unknown\".len()"
        );
    }

    // ── Notification helper builders ───────────────────────────────

    fn did_change_notification(uri: &str, text: &str, version: i32) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }
        })
        .to_string()
    }

    fn did_save_notification(uri: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": {
                "textDocument": { "uri": uri }
            }
        })
        .to_string()
    }

    fn did_close_notification(uri: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {
                "textDocument": { "uri": uri }
            }
        })
        .to_string()
    }

    fn initialized_notification() -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })
        .to_string()
    }

    fn document_symbol_request(id: u64, uri: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/documentSymbol",
            "params": { "textDocument": { "uri": uri } }
        })
        .to_string()
    }

    fn workspace_symbol_request(id: u64, query: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "workspace/symbol",
            "params": { "query": query }
        })
        .to_string()
    }

    fn workspace_folders_change(added: &[(&str, &str)], removed: &[(&str, &str)]) -> String {
        let added_json: Vec<Value> = added
            .iter()
            .map(|(uri, name)| serde_json::json!({"uri": uri, "name": name}))
            .collect();
        let removed_json: Vec<Value> = removed
            .iter()
            .map(|(uri, name)| serde_json::json!({"uri": uri, "name": name}))
            .collect();
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": {
                "event": {
                    "added": added_json,
                    "removed": removed_json
                }
            }
        })
        .to_string()
    }

    // ── Tests: notification dispatch ───────────────────────────────

    #[test]
    fn test_did_change_updates_content_and_publishes_diagnostics() {
        let uri = "file:///tmp/test.yX4Za";
        let text_v1 = "fn hello\n";
        let text_v2 = "fn goodbye\nfn extra_line\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text_v1)));
        input.extend(frame(&did_change_notification(uri, text_v2, 2)));
        // Hover on 'goodbye' (line 0, col 3) — only valid after didChange
        input.extend(frame(&hover_request(2, uri, 0, 3)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        // Verify content was updated via hover
        let hover = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("hover response");
        let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            value.contains("goodbye"),
            "didChange should update content, got: {value}"
        );

        // Verify diagnostics published from both didOpen and didChange
        let diag_count = messages
            .iter()
            .filter(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .count();
        assert!(
            diag_count >= 2,
            "Expected diagnostics from both didOpen and didChange, got {diag_count}"
        );

        // Verify the latest diagnostic reflects the updated line count
        let last_diag = messages
            .iter()
            .rfind(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .expect("at least one diagnostic");
        let msg = last_diag["params"]["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or("");
        assert!(
            msg.contains("2 lines"),
            "After didChange, diagnostic should reflect 2 lines, got: {msg}"
        );
    }

    #[test]
    fn test_did_save_publishes_diagnostics() {
        let mut args = default_args();
        args.advertise_save = true;
        args.diagnostics_on_save = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&did_save_notification(uri)));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        // With diagnostics_on_save, didOpen should NOT publish diagnostics,
        // but didSave should.
        let diags: Vec<&Value> = messages
            .iter()
            .filter(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .collect();

        assert_eq!(
            diags.len(),
            1,
            "Expected exactly 1 diagnostic (from didSave, not didOpen)"
        );
        assert_eq!(diags[0]["params"]["uri"], uri);
    }

    #[test]
    fn test_did_close_removes_document() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Hover should work before close
        input.extend(frame(&hover_request(2, uri, 0, 3)));
        input.extend(frame(&did_close_notification(uri)));
        // Hover should return null after close (document removed)
        input.extend(frame(&hover_request(3, uri, 0, 3)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(default_args(), &input);

        let hover1 = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("first hover response");
        assert!(
            hover1["result"].is_object(),
            "Hover before close should have result"
        );

        let hover2 = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("second hover response");
        assert!(
            hover2["result"].is_null(),
            "Hover after close should be null"
        );
    }

    /// Regression test for bug 49: a document loaded from disk via
    /// `--scan-roots` must survive a `didClose`. A real language server keeps
    /// its workspace index after close — it drops the in-memory overlay and
    /// reverts to on-disk content rather than forgetting the file. Catenary's
    /// enrichment opens then closes each file it touches, so an
    /// eviction-on-close silently dropped cross-file edges. A purely synthetic
    /// `didOpen` doc with no backing file is still removed, as before.
    #[test]
    fn test_did_close_retains_scanned_document() {
        let dir = std::env::temp_dir().join(format!("mockls_close_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scan dir");

        let file = dir.join(format!("core.{MOCK_LANG_A}"));
        std::fs::write(&file, "fn shared()\n").expect("write core file");
        let dir_str = dir.to_str().expect("valid path");
        let file_uri = format!("file://{}", file.to_str().expect("valid path"));
        // A synthetic doc with no backing file under the scanned dir.
        let ghost_uri = format!("file://{dir_str}/ghost.{MOCK_LANG_A}");

        let init_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": format!("file://{dir_str}")
            }
        })
        .to_string();

        let mut input = frame(&init_req);
        // Mirror enrichment: open then close the disk-backed file.
        input.extend(frame(&did_open_notification(&file_uri, "fn shared()\n")));
        input.extend(frame(&did_close_notification(&file_uri)));
        // Open then close a synthetic doc with no disk backing.
        input.extend(frame(&did_open_notification(&ghost_uri, "fn phantom()\n")));
        input.extend(frame(&did_close_notification(&ghost_uri)));
        input.extend(frame(&workspace_symbol_request(2, "")));
        input.extend(frame(&shutdown_request(99)));

        let mut args = default_args();
        args.scan_roots = true;
        let messages = run_server_with(args, &input);

        let _ = std::fs::remove_dir_all(&dir);

        let ws_resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("workspace/symbol response with id=2");
        let syms = ws_resp["result"].as_array().expect("result array");
        let names: Vec<&str> = syms.iter().filter_map(|s| s["name"].as_str()).collect();

        assert!(
            names.contains(&"shared"),
            "disk-backed `shared` must survive didClose, got: {names:?}"
        );
        assert!(
            !names.contains(&"phantom"),
            "synthetic `phantom` (no disk file) must be removed on didClose, got: {names:?}"
        );
    }

    #[test]
    fn test_workspace_folder_remove_cleans_documents() {
        let mut args = default_args();
        args.workspace_folders = true;
        args.scan_roots = true;

        let uri = "file:///tmp/folder_a/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        // Verify document is accessible
        input.extend(frame(&hover_request(2, uri, 0, 3)));
        // Remove the workspace folder containing the document
        input.extend(frame(&workspace_folders_change(
            &[],
            &[("file:///tmp/folder_a", "folder_a")],
        )));
        // Verify document is gone
        input.extend(frame(&hover_request(3, uri, 0, 3)));
        input.extend(frame(&shutdown_request(4)));

        let messages = run_server_with(args, &input);

        let hover1 = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("first hover response");
        assert!(
            hover1["result"].is_object(),
            "Hover before folder remove should work"
        );

        let hover2 = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(3))
            .expect("second hover response");
        assert!(
            hover2["result"].is_null(),
            "Hover after folder remove should be null"
        );
    }

    // ── Tests: diagnostics flag combinations ───────────────────────

    #[test]
    fn test_diagnostics_on_save_suppresses_did_open() {
        let mut args = default_args();
        args.diagnostics_on_save = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        let has_diag = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
        });
        assert!(
            !has_diag,
            "diagnostics_on_save should suppress didOpen diagnostics"
        );
    }

    #[test]
    fn test_no_push_diagnostics_suppresses_all() {
        let mut args = default_args();
        args.no_push_diagnostics = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&did_change_notification(uri, "fn world\n", 2)));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        let has_diag = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
        });
        assert!(
            !has_diag,
            "no_push_diagnostics should suppress all push diagnostics"
        );
    }

    /// `--push-empty` publishes on the push path but with an EMPTY diagnostics
    /// array — the explicit, evidence-backed clean of a push-only server
    /// (misc 153). Distinct from `--no-push-diagnostics`, which publishes
    /// nothing at all.
    #[test]
    fn test_push_empty_publishes_empty_set() {
        let mut args = default_args();
        args.push_empty = true;
        let uri = "file:///tmp/test.yX4Za";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, "fn hello\n")));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        let publishes: Vec<&Value> = messages
            .iter()
            .filter(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .collect();
        assert!(
            !publishes.is_empty(),
            "push-empty must still publish (an explicit clean, not silence)"
        );
        for publish in publishes {
            let diags = publish
                .pointer("/params/diagnostics")
                .and_then(Value::as_array)
                .expect("publish carries a diagnostics array");
            assert!(
                diags.is_empty(),
                "push-empty publishes an empty set, got: {diags:?}"
            );
        }
    }

    #[test]
    fn test_publish_version_in_diagnostics() {
        let mut args = default_args();
        args.publish_version = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        let diag = messages
            .iter()
            .find(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .expect("publishDiagnostics notification");

        assert!(
            diag["params"].get("version").is_some(),
            "publish_version should include version field"
        );
        assert_eq!(
            diag["params"]["version"], 1,
            "Version should match didOpen version"
        );
    }

    #[test]
    fn test_report_open_count_in_diagnostics() {
        let mut args = default_args();
        args.report_open_count = true;
        let uri_a = "file:///tmp/a.yX4Za";
        let uri_b = "file:///tmp/b.yX4Za";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_a, "line1\n")));
        input.extend(frame(&did_open_notification(uri_b, "line1\nline2\n")));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        let diags: Vec<&Value> = messages
            .iter()
            .filter(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .collect();

        // After opening b, diagnostics should be re-published for both
        let has_both = diags.iter().any(|d| d["params"]["uri"] == uri_a)
            && diags.iter().any(|d| d["params"]["uri"] == uri_b);
        assert!(
            has_both,
            "report_open_count should re-publish for all open documents"
        );

        // The latest diagnostic for b should include "2 open"
        let b_diag = diags
            .iter()
            .rfind(|d| d["params"]["uri"] == uri_b)
            .expect("diagnostic for b");
        let msg = b_diag["params"]["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or("");
        assert!(
            msg.contains("2 open"),
            "Should report 2 open files, got: {msg}"
        );
    }

    // ── Tests: initialize capability flags ─────────────────────────

    #[test]
    fn test_initialize_no_type_hierarchy_flag() {
        let mut args = default_args();
        args.no_type_hierarchy = true;

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);
        let caps = &messages[0]["result"]["capabilities"];
        assert!(
            caps.get("typeHierarchyProvider").is_none() || caps["typeHierarchyProvider"].is_null(),
            "no_type_hierarchy should exclude typeHierarchyProvider"
        );
    }

    #[test]
    fn test_initialize_no_rename_flag() {
        let mut args = default_args();
        args.no_rename = true;

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);
        let caps = &messages[0]["result"]["capabilities"];
        assert!(
            caps.get("renameProvider").is_none() || caps["renameProvider"].is_null(),
            "no_rename should exclude renameProvider"
        );
    }

    #[test]
    fn test_initialize_pull_diagnostics_capability() {
        let mut args = default_args();
        args.pull_diagnostics = true;

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);
        let caps = &messages[0]["result"]["capabilities"];
        assert!(
            caps["diagnosticProvider"].is_object(),
            "pull_diagnostics should advertise diagnosticProvider"
        );
    }

    // ── Tests: progress and lifecycle notifications ────────────────

    #[test]
    fn test_progress_on_change_sends_progress_tokens() {
        let mut args = default_args();
        args.progress_on_change = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&did_change_notification(uri, "fn world\n", 2)));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_wait(args, &input, 300);

        let has_create = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("window/workDoneProgress/create")
        });
        assert!(has_create, "progress_on_change should send progress create");

        let has_begin = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("$/progress")
                && m["params"]["value"]["kind"] == "begin"
                && m["params"]["value"]["title"] == "Checking"
        });
        assert!(has_begin, "progress_on_change should send Checking begin");

        let has_end = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("$/progress")
                && m["params"]["value"]["kind"] == "end"
        });
        assert!(has_end, "progress_on_change should send progress end");
    }

    #[test]
    fn test_register_file_watchers_on_initialized() {
        let mut args = default_args();
        args.register_file_watchers = true;

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&initialized_notification()));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        let register = messages
            .iter()
            .find(|m| m.get("method").and_then(Value::as_str) == Some("client/registerCapability"))
            .expect("Should send registerCapability for file watchers");

        let registrations = register["params"]["registrations"]
            .as_array()
            .expect("registrations array");
        assert_eq!(
            registrations[0]["method"],
            "workspace/didChangeWatchedFiles"
        );
        let watchers = registrations[0]["registerOptions"]["watchers"]
            .as_array()
            .expect("watchers array");
        assert_eq!(watchers[0]["globPattern"], "**/*");
    }

    #[test]
    fn test_send_configuration_request_after_initialize() {
        let mut args = default_args();
        args.send_configuration_request = true;

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&shutdown_request(2)));

        let messages = run_server_with(args, &input);

        let config_req = messages
            .iter()
            .find(|m| m.get("method").and_then(Value::as_str) == Some("workspace/configuration"))
            .expect("Should send workspace/configuration request");

        let items = config_req["params"]["items"]
            .as_array()
            .expect("items array");
        assert_eq!(items[0]["section"], "mockls");
    }

    #[test]
    fn test_flycheck_publishes_diagnostics_and_progress() {
        let mut args = default_args();
        args.advertise_save = true;
        args.flycheck_command = Some("true".to_string());
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn hello\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&did_save_notification(uri)));
        input.extend(frame(&shutdown_request(2)));

        // Wait for the flycheck thread to complete
        let messages = run_server_wait(args, &input, 500);

        // didOpen publishes one, flycheck publishes another
        let diag_count = messages
            .iter()
            .filter(|m| {
                m.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            })
            .count();
        assert!(
            diag_count >= 2,
            "Expected diagnostics from both didOpen and flycheck, got {diag_count}"
        );

        let has_flycheck_begin = messages.iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some("$/progress")
                && m["params"]["value"]["kind"] == "begin"
                && m["params"]["value"]["title"] == "Flycheck"
        });
        assert!(has_flycheck_begin, "Flycheck should send progress begin");
    }

    // ── Tests: document and workspace symbols ──────────────────────

    #[test]
    fn test_document_symbols_response() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn my_func\nstruct MyStruct\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&document_symbol_request(2, uri)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("documentSymbol response");
        assert!(resp["error"].is_null(), "Expected no error");
        let symbols = resp["result"].as_array().expect("result array");
        assert!(symbols.len() >= 2, "Expected at least 2 symbols");

        let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(names.contains(&"my_func"), "Should contain my_func");
        assert!(names.contains(&"MyStruct"), "Should contain MyStruct");

        let func = symbols
            .iter()
            .find(|s| s["name"] == "my_func")
            .expect("my_func symbol");
        assert_eq!(func["kind"], 12, "fn → Function(12)");
        assert!(func["range"].is_object(), "Symbol should have range");
        assert!(
            func["selectionRange"].is_object(),
            "Symbol should have selectionRange"
        );
    }

    #[test]
    fn test_workspace_symbols_response() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn my_func\nstruct MyStruct\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&workspace_symbol_request(2, "")));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("workspace/symbol response");
        assert!(resp["error"].is_null(), "Expected no error");
        let symbols = resp["result"].as_array().expect("result array");
        assert!(symbols.len() >= 2, "Expected at least 2 symbols");

        let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(names.contains(&"my_func"), "Should contain my_func");
        assert!(names.contains(&"MyStruct"), "Should contain MyStruct");

        // Workspace symbols should have location with uri and range
        let func = symbols
            .iter()
            .find(|s| s["name"] == "my_func")
            .expect("my_func symbol");
        assert!(
            func["location"].is_object(),
            "Workspace symbol should have location"
        );
        assert_eq!(func["location"]["uri"], uri);
        assert!(
            func["location"]["range"].is_object(),
            "Workspace symbol location should have range"
        );
    }

    #[test]
    fn test_workspace_symbols_query_filter() {
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn alpha\nfn beta\nstruct Gamma\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&workspace_symbol_request(2, "alpha")));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(default_args(), &input);

        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("workspace/symbol response");
        let symbols = resp["result"].as_array().expect("result array");
        assert_eq!(
            symbols.len(),
            1,
            "Query 'alpha' should return exactly 1 symbol"
        );
        assert_eq!(symbols[0]["name"], "alpha");
    }

    #[test]
    fn test_workspace_symbols_no_empty_query() {
        let mut args = default_args();
        args.no_empty_query = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn my_func\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&workspace_symbol_request(2, "")));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(args, &input);

        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("workspace/symbol response");
        let symbols = resp["result"].as_array().expect("result array");
        assert!(
            symbols.is_empty(),
            "no_empty_query should return empty for empty query"
        );
    }

    #[test]
    fn test_workspace_symbols_resolve_provider_uri_only() {
        let mut args = default_args();
        args.resolve_provider = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn my_func\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&workspace_symbol_request(2, "")));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(args, &input);

        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("workspace/symbol response");
        let symbols = resp["result"].as_array().expect("result array");
        assert!(!symbols.is_empty(), "Should return symbols");

        // With resolve_provider, location should have URI but no range
        let sym = &symbols[0];
        assert_eq!(sym["location"]["uri"], uri);
        assert!(
            sym["location"].get("range").is_none() || sym["location"]["range"].is_null(),
            "resolve_provider should omit range from workspace/symbol"
        );
    }

    #[test]
    fn test_workspace_symbol_resolve_finds_correct_symbol() {
        let mut args = default_args();
        args.resolve_provider = true;
        let uri = "file:///tmp/test.yX4Za";
        let text = "fn alpha\nfn beta\n";

        let unresolved = serde_json::json!({
            "name": "beta",
            "kind": 12,
            "location": { "uri": uri }
        });

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&workspace_symbol_resolve_request(2, &unresolved)));
        input.extend(frame(&shutdown_request(3)));

        let messages = run_server_with(args, &input);

        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("resolve response");
        assert!(resp["error"].is_null(), "Expected no error");
        let result = &resp["result"];
        assert_eq!(result["name"], "beta");
        assert!(
            result["location"]["range"].is_object(),
            "Resolved symbol should have range"
        );
        // beta is on line 1
        assert_eq!(
            result["location"]["range"]["start"]["line"], 1,
            "beta should be on line 1"
        );
    }

    // ── Tests: structural mutant kills ─────────────────────────────

    /// Extracted `check_drop_after` makes the response counter testable
    /// without hitting `process::exit`. Kills `replace += with *=`.
    #[test]
    fn test_check_drop_after_triggers_on_threshold() {
        let (writer, _) = buffer_writer();
        let mut args = default_args();
        args.drop_after = Some(3);
        let mut server = MockServer::new(args, writer);

        assert!(!server.check_drop_after(), "count=1, below threshold");
        assert!(!server.check_drop_after(), "count=2, below threshold");
        assert!(server.check_drop_after(), "count=3, should trigger");
        // Past threshold: still true
        assert!(server.check_drop_after(), "count=4, past threshold");
    }

    /// Without `drop_after`, `check_drop_after` always returns false.
    #[test]
    fn test_check_drop_after_no_limit() {
        let (writer, _) = buffer_writer();
        let mut server = MockServer::new(default_args(), writer);

        for _ in 0..10 {
            assert!(
                !server.check_drop_after(),
                "Should never trigger without drop_after"
            );
        }
    }

    /// Notification log uses different JSON shapes for watched-files vs
    /// other notifications. Kills `replace == with !=` on the method check.
    #[test]
    fn test_notification_log_records_method_and_uri() {
        let log_path = std::env::temp_dir().join(format!(
            "mockls_test_notif_log_{}.jsonl",
            std::process::id()
        ));
        let log_str = log_path.to_str().expect("valid temp path");

        let mut args = default_args();
        args.notification_log = Some(log_str.to_string());
        let uri = "file:///tmp/test.yX4Za";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, "fn hello\n")));
        input.extend(frame(&shutdown_request(2)));

        run_server_with(args, &input);

        let log_content = std::fs::read_to_string(&log_path).expect("read notification log");
        let _ = std::fs::remove_file(&log_path);

        // didOpen should be logged with "uri" field (not "changes")
        let did_open_line = log_content
            .lines()
            .find(|l| l.contains("didOpen"))
            .expect("didOpen entry in notification log");
        let entry: Value =
            serde_json::from_str(did_open_line).expect("valid JSON in notification log");
        assert_eq!(entry["method"], "textDocument/didOpen");
        assert_eq!(
            entry["uri"], uri,
            "didOpen should have uri field, not changes"
        );
        assert!(
            entry.get("changes").is_none(),
            "didOpen should not have changes field"
        );
    }

    // ── Tests: scan_directory + rebuild_imports ────────────────────

    /// `scan_directory` indexes `.{name}` files from disk, skipping
    /// hidden dirs and non-matching extensions. Exercises both the
    /// extension filter (`== Some(name)`) and the body itself.
    #[test]
    fn test_scan_directory_indexes_matching_files() {
        let dir = std::env::temp_dir().join(format!("mockls_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".hidden")).expect("create hidden dir");
        std::fs::create_dir_all(dir.join("sub")).expect("create sub dir");

        // Matching files
        std::fs::write(dir.join(format!("a.{MOCK_LANG_A}")), "fn alpha()\n").expect("write a.mock");
        std::fs::write(dir.join(format!("sub/b.{MOCK_LANG_A}")), "fn beta()\n")
            .expect("write sub/b.mock");

        // Non-matching: wrong extension
        std::fs::write(dir.join("c.txt"), "fn gamma()\n").expect("write c.txt");

        // Hidden directory: should be skipped
        std::fs::write(dir.join(format!(".hidden/d.{MOCK_LANG_A}")), "fn delta()\n")
            .expect("write hidden mock");

        let dir_str = dir.to_str().expect("valid path");
        let root_uri = format!("file://{dir_str}");
        let init_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": root_uri
            }
        })
        .to_string();

        let ws_sym_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "workspace/symbol",
            "params": { "query": "" }
        })
        .to_string();

        let mut input = frame(&init_req);
        input.extend(frame(&ws_sym_req));
        input.extend(frame(&shutdown_request(99)));

        let mut args = default_args();
        args.scan_roots = true;
        let messages = run_server_with(args, &input);

        let _ = std::fs::remove_dir_all(&dir);

        let ws_resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("workspace/symbol response with id=2");
        let syms = ws_resp["result"].as_array().expect("result array");

        let names: Vec<&str> = syms.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(
            names.contains(&"alpha"),
            "alpha from a.mock should be indexed"
        );
        assert!(
            names.contains(&"beta"),
            "beta from sub/b.mock should be indexed"
        );
        assert!(
            !names.contains(&"gamma"),
            "gamma from c.txt (wrong extension) should not be indexed"
        );
        assert!(
            !names.contains(&"delta"),
            "delta from .hidden/ should not be indexed"
        );
    }

    /// `rebuild_imports` populates the import map so that definition
    /// requests resolve via import scope rather than cross-file fallback.
    /// `documents` is a `BTreeMap`, so cross-file search visits keys in
    /// sorted order. The wrong target (`aaa`) sorts before the import
    /// target (`zzz`), so a no-op `rebuild_imports` deterministically
    /// resolves to the wrong file.
    #[test]
    fn test_rebuild_imports_scoped_resolution() {
        // Wrong target — sorts first in BTreeMap iteration
        let uri_wrong = "file:///tmp/aaa.yX4Za";
        let text_wrong = "fn helper()\n";

        // Correct target — sorts last
        let uri_right = "file:///tmp/zzz.yX4Za";
        let text_right = "fn helper()\n";

        // Caller imports from zzz specifically
        let uri_caller = "file:///tmp/caller.yX4Za";
        let text_caller = "from zzz import helper\nhelper\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_wrong, text_wrong)));
        input.extend(frame(&did_open_notification(uri_right, text_right)));
        input.extend(frame(&did_open_notification(uri_caller, text_caller)));
        // Definition on 'helper' in caller (line 1, col 0)
        input.extend(frame(&definition_request(2, uri_caller, 1, 0)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);

        let def = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("definition response with id=2");
        assert!(def["error"].is_null(), "Expected no error");
        assert_eq!(
            def["result"]["uri"], uri_right,
            "Import should resolve to zzz, not cross-file fallback to aaa"
        );
    }

    // ── Tests: type hierarchy edge cases ──────────────────────────

    /// `handle_type_hierarchy_prepare` maps type keywords to correct
    /// LSP symbol kinds. Exercises the kind-mapping branches.
    #[test]
    fn test_type_hierarchy_prepare_kind_mapping() {
        let uri = "file:///tmp/kinds.yX4Za";
        // Line 0: "interface Iface"   kind=11
        // Line 1: "trait Tr"          kind=11
        // Line 2: "class Cls"         kind=5
        // Line 3: "enum En"           kind=10
        // Line 4: "struct St"         kind=23 (fallback)
        let text = "interface Iface\ntrait Tr\nclass Cls\nenum En\nstruct St\n";

        let cases: &[(u64, u64, &str, u64)] = &[
            (0, 10, "Iface", 11), // interface
            (1, 6, "Tr", 11),     // trait
            (2, 6, "Cls", 5),     // class
            (3, 5, "En", 10),     // enum
            (4, 7, "St", 23),     // struct (fallback kind)
        ];

        for (line, col, expected_name, expected_kind) in cases {
            let mut input = frame(&initialize_request(1));
            input.extend(frame(&did_open_notification(uri, text)));
            input.extend(frame(&prepare_type_hierarchy_request(2, uri, *line, *col)));
            input.extend(frame(&shutdown_request(99)));

            let messages = run_server_with(default_args(), &input);

            let resp = messages
                .iter()
                .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
                .expect("prepareTypeHierarchy response");
            let items = resp["result"].as_array().expect("result array");
            assert_eq!(items.len(), 1, "line {line}");
            assert_eq!(
                items[0]["name"], *expected_name,
                "name mismatch on line {line}"
            );
            assert_eq!(
                items[0]["kind"], *expected_kind,
                "kind mismatch on line {line} for {expected_name}"
            );
        }
    }

    /// Subtypes from multiple documents: `find_type_declaration` must
    /// locate the parent across files and return correct uri/line/kind.
    /// Trailing ` {` after type names kills `== → !=` mutants on the
    /// `take_while` predicates in both the supertypes handler and
    /// `find_type_declaration`.
    #[test]
    fn test_supertypes_cross_file() {
        // Parent type in one document
        let uri_base = "file:///tmp/base.yX4Za";
        let text_base = "trait Serializable {\n";

        // Child type in another document
        let uri_child = "file:///tmp/child.yX4Za";
        // Line 0: "class JsonCodec implements Serializable {" (len=42)
        let text_child = "class JsonCodec implements Serializable {\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_base, text_base)));
        input.extend(frame(&did_open_notification(uri_child, text_child)));
        // Prepare on "JsonCodec" (line 0, char 6)
        input.extend(frame(&prepare_type_hierarchy_request(2, uri_child, 0, 6)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);

        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepareTypeHierarchy response");
        let items = prepare["result"].as_array().expect("result array");
        assert_eq!(items[0]["name"], "JsonCodec");

        // Request supertypes
        let codec_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri_base, text_base)));
        input2.extend(frame(&did_open_notification(uri_child, text_child)));
        input2.extend(frame(&supertypes_request(11, codec_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);

        let supertypes = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("supertypes response");
        let parents = supertypes["result"].as_array().expect("result array");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0]["name"], "Serializable");
        assert_eq!(parents[0]["kind"], 11, "trait maps to kind 11");
        assert_eq!(
            parents[0]["uri"], uri_base,
            "parent declaration is in base.yX4Za"
        );
        assert_eq!(parents[0]["range"]["start"]["line"], 0);
        assert_eq!(parents[0]["range"]["start"]["character"], 0);
        assert_eq!(
            parents[0]["range"]["end"]["character"], 20,
            "end = len(\"trait Serializable {{\")"
        );
        assert_eq!(parents[0]["selectionRange"]["start"]["line"], 0);
        assert_eq!(parents[0]["selectionRange"]["end"]["character"], 20);
    }

    /// Supertypes synthetic entry: when the parent type is not found
    /// in any document, a synthetic entry is returned with the child's
    /// location and kind=5.
    #[test]
    fn test_supertypes_synthetic_when_parent_missing() {
        let uri = "file:///tmp/orphan.yX4Za";
        // Line 0: "class Orphan extends Unknown" (len=28)
        let text = "class Orphan extends Unknown\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&prepare_type_hierarchy_request(2, uri, 0, 6)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);
        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepare response");
        let items = prepare["result"].as_array().expect("result array");

        let orphan_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri, text)));
        input2.extend(frame(&supertypes_request(11, orphan_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);
        let supertypes = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("supertypes response");
        let parents = supertypes["result"].as_array().expect("result array");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0]["name"], "Unknown");
        assert_eq!(
            parents[0]["kind"], 5,
            "synthetic parent gets default kind 5 (class)"
        );
        assert_eq!(parents[0]["uri"], uri, "synthetic parent uses child's URI");
        assert_eq!(
            parents[0]["range"]["start"]["line"], 0,
            "synthetic parent uses child's line"
        );
    }

    // ── Tests: outgoing calls edge cases ──────────────────────────

    /// Outgoing calls across files: the callee is defined in a
    /// different document. Verifies `to.uri` points to the correct
    /// file and `to.range` reflects the callee's declaration line.
    ///
    /// The caller's declaration line mentions "utility" in a comment
    /// so that `(caller_line * 1)..body_end` (which includes the
    /// declaration line) produces an extra `fromRange` — killing the
    /// `replace + with *` mutant on the for-loop range.
    #[test]
    fn test_outgoing_calls_cross_file() {
        let uri_lib = "file:///tmp/lib.yX4Za";
        // Line 0: "fn utility()"  (len=12)
        let text_lib = "fn utility()\n";

        let uri_main = "file:///tmp/main.yX4Za";
        // Line 0: "fn entry() -- utility"  (len=21, mentions callee)
        // Line 1: "    utility"            (len=11)
        let text_main = "fn entry() -- utility\n    utility\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri_lib, text_lib)));
        input.extend(frame(&did_open_notification(uri_main, text_main)));
        input.extend(frame(&prepare_call_hierarchy_request(2, uri_main, 0, 3)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);
        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepareCallHierarchy response");
        let items = prepare["result"].as_array().expect("result array");
        assert_eq!(items[0]["name"], "entry");

        let entry_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri_lib, text_lib)));
        input2.extend(frame(&did_open_notification(uri_main, text_main)));
        input2.extend(frame(&outgoing_calls_request(11, entry_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);
        let outgoing = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("outgoingCalls response");
        let calls = outgoing["result"].as_array().expect("result array");
        assert_eq!(calls.len(), 1, "entry calls utility");
        assert_eq!(calls[0]["to"]["name"], "utility");
        assert_eq!(
            calls[0]["to"]["uri"], uri_lib,
            "utility is defined in lib.yX4Za"
        );
        assert_eq!(calls[0]["to"]["range"]["start"]["line"], 0);
        assert_eq!(
            calls[0]["to"]["range"]["end"]["character"], 12,
            "end = len(\"fn utility()\")"
        );
        assert_eq!(calls[0]["to"]["selectionRange"]["start"]["character"], 0);
        assert_eq!(calls[0]["to"]["selectionRange"]["end"]["character"], 12);

        let from_ranges = calls[0]["fromRanges"].as_array().expect("fromRanges");
        assert_eq!(
            from_ranges.len(),
            1,
            "only the body call site, not the declaration line"
        );
        assert_eq!(from_ranges[0]["start"]["line"], 1);
        assert_eq!(from_ranges[0]["end"]["character"], 11);
    }

    /// Outgoing calls: the caller's own name must not appear in results
    /// (even if the body mentions it). Also verifies multiple call sites
    /// produce multiple fromRanges.
    #[test]
    fn test_outgoing_calls_excludes_self_and_multiple_ranges() {
        let uri = "file:///tmp/multi.yX4Za";
        // Line 0: "fn worker()"  (len=11)
        // Line 1: "fn boss()"    (len=9)
        // Line 2: "    worker"   (len=10) — first call
        // Line 3: "    boss"     (len=8) — recursive mention (should be excluded)
        // Line 4: "    worker"   (len=10) — second call
        let text = "fn worker()\nfn boss()\n    worker\n    boss\n    worker\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&prepare_call_hierarchy_request(2, uri, 1, 3)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);
        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepare response");
        let items = prepare["result"].as_array().expect("result array");
        assert_eq!(items[0]["name"], "boss");

        let boss_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri, text)));
        input2.extend(frame(&outgoing_calls_request(11, boss_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);
        let outgoing = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("outgoingCalls response");
        let calls = outgoing["result"].as_array().expect("result array");
        assert_eq!(calls.len(), 1, "only worker, not boss (self-exclusion)");
        assert_eq!(calls[0]["to"]["name"], "worker");

        let from_ranges = calls[0]["fromRanges"].as_array().expect("fromRanges");
        assert_eq!(from_ranges.len(), 2, "worker called twice from boss body");
        assert_eq!(from_ranges[0]["start"]["line"], 2);
        assert_eq!(from_ranges[1]["start"]["line"], 4);
    }

    /// Deprecated tags propagate through type hierarchy subtypes and
    /// outgoing calls. Exercises the `@deprecated` annotation checks
    /// in `handle_type_hierarchy_subtypes` and `handle_outgoing_calls`.
    #[test]
    fn test_deprecated_propagation_subtypes_and_calls() {
        let uri = "file:///tmp/dep.yX4Za";
        // Line 0: "interface Base"
        // Line 1: "class OldImpl implements Base @deprecated"  (len=41)
        // Line 2: "class NewImpl implements Base"              (len=29)
        let text = "interface Base\nclass OldImpl implements Base @deprecated\nclass NewImpl implements Base\n";

        let mut input = frame(&initialize_request(1));
        input.extend(frame(&did_open_notification(uri, text)));
        input.extend(frame(&prepare_type_hierarchy_request(2, uri, 0, 10)));
        input.extend(frame(&shutdown_request(99)));

        let messages = run_server_with(default_args(), &input);
        let prepare = messages
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(2))
            .expect("prepare response");
        let items = prepare["result"].as_array().expect("result array");

        let base_item = &items[0];
        let mut input2 = frame(&initialize_request(10));
        input2.extend(frame(&did_open_notification(uri, text)));
        input2.extend(frame(&subtypes_request(11, base_item)));
        input2.extend(frame(&shutdown_request(99)));

        let messages2 = run_server_with(default_args(), &input2);
        let subtypes = messages2
            .iter()
            .find(|m| m.get("id").and_then(Value::as_u64) == Some(11))
            .expect("subtypes response");
        let children = subtypes["result"].as_array().expect("result array");
        assert_eq!(children.len(), 2);

        let old_impl = children
            .iter()
            .find(|c| c["name"] == "OldImpl")
            .expect("OldImpl");
        assert_eq!(
            old_impl["tags"],
            serde_json::json!([1]),
            "OldImpl should have DEPRECATED tag"
        );

        let new_impl = children
            .iter()
            .find(|c| c["name"] == "NewImpl")
            .expect("NewImpl");
        assert!(
            new_impl.get("tags").is_none() || new_impl["tags"].is_null(),
            "NewImpl should not have DEPRECATED tag"
        );
    }

    // ── Tests: utility function unit tests ────────────────────────

    #[test]
    fn test_extract_position_valid_and_missing() {
        let valid = serde_json::json!({
            "textDocument": { "uri": "file:///tmp/test.rs" },
            "position": { "line": 5, "character": 10 }
        });
        assert_eq!(
            extract_position(&valid),
            Some(("file:///tmp/test.rs", 5, 10))
        );

        // Missing textDocument → None
        let no_td = serde_json::json!({ "position": { "line": 0, "character": 0 } });
        assert_eq!(extract_position(&no_td), None);

        // Missing position → None
        let no_pos = serde_json::json!({ "textDocument": { "uri": "file:///x" } });
        assert_eq!(extract_position(&no_pos), None);
    }

    #[test]
    fn test_location_json_all_fields() {
        let loc = location_json("file:///tmp/test.rs", 3, 5, 10);
        assert_eq!(loc["uri"], "file:///tmp/test.rs");
        assert_eq!(loc["range"]["start"]["line"], 3);
        assert_eq!(loc["range"]["start"]["character"], 5);
        assert_eq!(loc["range"]["end"]["line"], 3);
        assert_eq!(loc["range"]["end"]["character"], 10);
    }

    #[test]
    fn test_extract_word_exact_values() {
        let content = "fn hello_world()\nfoo bar";
        // Middle of underscore name
        assert_eq!(extract_word(content, 0, 5), Some("hello_world".to_string()));
        // Start of line (keyword)
        assert_eq!(extract_word(content, 0, 0), Some("fn".to_string()));
        // Second line, second word
        assert_eq!(extract_word(content, 1, 4), Some("bar".to_string()));

        // Boundary: col == len → None
        assert_eq!(extract_word("hello", 0, 5), None);
        // Boundary: col == len-1 → word
        assert_eq!(extract_word("hello", 0, 4), Some("hello".to_string()));
        // Non-existent line
        assert_eq!(extract_word("hello", 1, 0), None);

        // Leading non-word chars: verify start offset arithmetic
        assert_eq!(
            extract_word("  hello_world  ", 0, 2),
            Some("hello_world".to_string())
        );
        // Non-word char at col → None
        assert_eq!(extract_word("a b", 0, 1), None);
    }

    #[test]
    fn test_extract_symbol_name_keyword_and_direct() {
        let content = "fn my_func()\nstruct MyStruct";
        // Cursor on keyword → resolve to following name
        assert_eq!(
            extract_symbol_name(content, 0, 0),
            Some("my_func".to_string())
        );
        assert_eq!(
            extract_symbol_name(content, 1, 0),
            Some("MyStruct".to_string())
        );
        // Cursor directly on name → return name
        assert_eq!(
            extract_symbol_name(content, 0, 3),
            Some("my_func".to_string())
        );
    }

    #[test]
    fn test_find_enclosing_function_cases() {
        // Basic: find enclosing fn with underscore name
        let content = "fn my_outer()\n    let x = 1\n";
        assert_eq!(
            find_enclosing_function(content, 1),
            Some(("my_outer".to_string(), 0))
        );

        // Nearest: inner shadows outer
        let content2 = "fn outer()\nfn inner()\n    body\n";
        assert_eq!(
            find_enclosing_function(content2, 2),
            Some(("inner".to_string(), 1))
        );

        // None: line 0 has nothing above it
        assert_eq!(find_enclosing_function("let x = 1\nfn foo()\n", 0), None);
    }

    #[test]
    fn test_is_word_char_coverage() {
        assert!(is_word_char(b'a'));
        assert!(is_word_char(b'Z'));
        assert!(is_word_char(b'0'));
        assert!(is_word_char(b'_'));
        assert!(!is_word_char(b' '));
        assert!(!is_word_char(b'+'));
        assert!(!is_word_char(b'('));
    }

    #[test]
    fn test_extract_types_edge_cases() {
        // Underscore in both name and type — kills `== → !=` on `_` checks
        let content = "let my_var: My_Type\nconst MY_CONST: Some_Type\n";
        let types = extract_types(content);
        assert_eq!(types.get("my_var").map(String::as_str), Some("My_Type"));
        assert_eq!(types.get("MY_CONST").map(String::as_str), Some("Some_Type"));
        assert_eq!(types.len(), 2);

        // No type annotation → empty
        let empty = extract_types("let x\nfn foo\n");
        assert!(empty.is_empty());

        // Verifies `colon_pos + 2` arithmetic
        let single = extract_types("let x: Foo\n");
        assert_eq!(single.get("x").map(String::as_str), Some("Foo"));
    }

    #[test]
    fn test_extract_symbols_hierarchy_and_ranges() {
        // Hierarchy: brace block creates parent/child
        let content = "struct Outer {\n    fn inner()\n}\n";
        let symbols = extract_symbols(content);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0]["name"], "Outer");
        let children = symbols[0]["children"].as_array().expect("children array");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "inner");
        assert_eq!(children[0]["kind"], 12);

        // Indent arithmetic: verify range and selectionRange
        // "    fn indented()" → indent=4, prefix_len=3, name_len=8
        let indented = extract_symbols("    fn indented()\n");
        assert_eq!(indented.len(), 1);
        let sym = &indented[0];
        assert_eq!(sym["range"]["start"]["character"], 4);
        assert_eq!(sym["selectionRange"]["start"]["character"], 7); // 4 + 3
        assert_eq!(sym["selectionRange"]["end"]["character"], 15); // 7 + 8

        // Underscore names — kills `|| → &&` and `== → !=` on `_`
        let underscore = extract_symbols("fn my_func\nlet my_var\n");
        let names: Vec<&str> = underscore
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert!(names.contains(&"my_func"));
        assert!(names.contains(&"my_var"));
    }

    #[test]
    fn test_parse_symbol_line_exhaustive() {
        assert_eq!(parse_symbol_line("fn foo"), Some((12, 3)));
        assert_eq!(parse_symbol_line("function foo"), Some((12, 9)));
        assert_eq!(parse_symbol_line("def foo"), Some((12, 4)));
        assert_eq!(parse_symbol_line("let x"), Some((13, 4)));
        assert_eq!(parse_symbol_line("const X"), Some((14, 6)));
        assert_eq!(parse_symbol_line("var x"), Some((13, 4)));
        assert_eq!(parse_symbol_line("struct S"), Some((23, 7)));
        assert_eq!(parse_symbol_line("class C"), Some((5, 6)));
        assert_eq!(parse_symbol_line("enum E"), Some((10, 5)));
        assert_eq!(parse_symbol_line("interface I"), Some((11, 10)));
        assert_eq!(parse_symbol_line("trait T"), Some((11, 6)));
        assert_eq!(parse_symbol_line("mod m"), Some((2, 4)));
        assert_eq!(parse_symbol_line("module m"), Some((2, 7)));
        assert_eq!(parse_symbol_line("type T"), Some((26, 5)));
        assert_eq!(parse_symbol_line("method m"), Some((6, 7)));
        assert_eq!(parse_symbol_line("field f"), Some((8, 6)));
        assert_eq!(parse_symbol_line("unknown"), None);
        assert_eq!(parse_symbol_line(""), None);
    }
}
