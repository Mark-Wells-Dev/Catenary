// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Append-only JSONL firehose sink.
//!
//! [`JsonlSink`] is a [`logging::Sink`](super::Sink) that writes one JSON
//! record per line to the file selected by the record's *scope*. It is the
//! storage half of the observability rewrite (workstream 27): the `SQLite`
//! `messages` firehose — which had no retention and wedged a long-running
//! daemon — is replaced by sharded append-only logs in the cache dir.
//!
//! **The file is the scope.** Every record carries a scope id resolved
//! daemon-side at write time, and that id selects the file. The cross-record
//! grouping the old flat `messages` table reconstructed at read time is now the
//! directory structure:
//!
//! ```text
//! <cache>/catenary/<instance_id>/
//! ├── mcp.jsonl                       # instance-global MCP heartbeat
//! ├── trace.jsonl                     # instance-global internal trace (level-filtered)
//! ├── servers/<server>.jsonl          # rootless server lifecycle (workspace/single-file tiers)
//! └── <encoded-cwd>/                  # one project root, path flattened CC-style
//!     ├── <server>.jsonl              #   autonomous lifecycle ($/progress, logMessage, spawn, init)
//!     ├── grep/<ts>_<uuid>.jsonl      #   one invocation = one file (cmd record + triggered LSP)
//!     ├── glob/<ts>_<uuid>.jsonl
//!     └── sessions/<session_id>.jsonl #   hook decisions · edits · diagnostics(+LSP) · sed(+LSP)
//! ```
//!
//! **Additive.** This ticket lands the sink, the [`Record`] schema, the [`Scope`]
//! resolver, and the path layout. Wiring it into the active sink set (alongside,
//! then in place of, `MessageDbSink`) is the firehose cutover (ticket 02).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use chrono::SecondsFormat;
use chrono::Utc;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use uuid::Uuid;

use super::LogEvent;
use super::Severity;
use super::Sink;
use crate::db::encode_cwd;

/// Max number of open append file handles kept warm.
///
/// Reopening a file per line would dominate the write cost; this keeps the hot
/// files (the active sessions/servers/streams) open. Beyond the cap, the
/// least-recently-used handle is closed. Sized generously — a busy daemon has a
/// handful of live scopes, not dozens.
const HANDLE_CACHE_CAP: usize = 64;

/// Which stateless search command produced a [`Scope::Search`] record. Selects
/// the per-tool subdirectory (`grep/` or `glob/`) under the cwd shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchTool {
    Grep,
    Glob,
}

impl SearchTool {
    /// Subdirectory name under the cwd shard.
    const fn dir_name(self) -> &'static str {
        match self {
            Self::Grep => "grep",
            Self::Glob => "glob",
        }
    }
}

/// Which instance-global stream an [`Scope::Instance`] record lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstanceStream {
    /// MCP heartbeat traffic (`mcp.jsonl`).
    Mcp,
    /// Internal trace events, level-filtered (`trace.jsonl`).
    Trace,
}

impl InstanceStream {
    /// Filename of the instance-global stream, directly under the firehose root.
    const fn file_name(self) -> &'static str {
        match self {
            Self::Mcp => "mcp.jsonl",
            Self::Trace => "trace.jsonl",
        }
    }
}

/// The top-level scope a record belongs to.
///
/// Each variant maps (via [`Scope::target`]) to exactly one JSONL file under the
/// firehose root plus a self-describing `scope_id` string. The file path is the
/// authoritative selector; `scope_id` is only for self-description when a record
/// is pulled out of its file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// Hook-correlated work (editing, `diagnostics`, `sed`), keyed by the
    /// session id. Lives at `<cwd>/sessions/<session_id>.jsonl`, or
    /// `sessions/<session_id>.jsonl` when no cwd is known.
    Session {
        cwd: Option<String>,
        session_id: String,
    },
    /// One stateless `grep`/`glob` invocation, keyed by a daemon-minted
    /// per-request UUID (no hook handoff — Decision 4). Lives at
    /// `<cwd>/<tool>/<ts>_<uuid>.jsonl`.
    Search {
        cwd: Option<String>,
        tool: SearchTool,
        ts: String,
        id: Uuid,
    },
    /// A server's *autonomous* lifecycle (`$/progress`, `window/logMessage`,
    /// spawn, `initialize`) — traffic no request triggered. A project-scoped
    /// (rootful) instance lives at `<cwd>/<server>.jsonl`; a rootless
    /// (workspace/single-file tier) instance lives at `servers/<server>.jsonl`.
    Server {
        server: String,
        scope_root: Option<String>,
    },
    /// Traffic no command or server owns — the instance-global streams.
    Instance { id: String, stream: InstanceStream },
}

impl Scope {
    /// Map the scope to its `(target_file, scope_id)`, rooted at the firehose
    /// root (`<cache>/catenary/<instance_id>`).
    fn target(&self, root: &Path) -> (PathBuf, String) {
        match self {
            Self::Session { cwd, session_id } => (
                cwd_base(root, cwd.as_deref())
                    .join("sessions")
                    .join(format!("{session_id}.jsonl")),
                session_id.clone(),
            ),
            Self::Search { cwd, tool, ts, id } => {
                let hex = id.simple().to_string();
                let name = if ts.is_empty() {
                    format!("{hex}.jsonl")
                } else {
                    format!("{ts}_{hex}.jsonl")
                };
                (
                    cwd_base(root, cwd.as_deref())
                        .join(tool.dir_name())
                        .join(name),
                    hex,
                )
            }
            Self::Server { server, scope_root } => scope_root.as_deref().map_or_else(
                || {
                    (
                        root.join("servers").join(format!("{server}.jsonl")),
                        server.clone(),
                    )
                },
                |r| {
                    (
                        root.join(encode_cwd(Path::new(r)))
                            .join(format!("{server}.jsonl")),
                        format!("{server}@{r}"),
                    )
                },
            ),
            Self::Instance { id, stream } => (root.join(stream.file_name()), id.clone()),
        }
    }

    /// The cwd shard string for the record body (empty when the scope has no
    /// project root).
    fn cwd(&self) -> &str {
        match self {
            Self::Session { cwd, .. } | Self::Search { cwd, .. } => cwd.as_deref().unwrap_or(""),
            Self::Server { scope_root, .. } => scope_root.as_deref().unwrap_or(""),
            Self::Instance { .. } => "",
        }
    }
}

/// Resolve the scope of an event, daemon-side, at write time.
///
/// Generalizes the degenerate selector the old DB sink used
/// (`session_id.unwrap_or(instance_id)`, `message_db.rs`) into the four-way
/// `session → search → server → instance` rule:
///
/// - **Session** — the event carries a `session_id` (hook-correlated work, plus
///   any LSP it triggered, which inherits the session span).
/// - **Search** — a stateless `grep`/`glob` invocation. The daemon mints a
///   per-request UUID at the IPC boundary and tags the invocation's events (the
///   command record and any LSP it triggers) with the structured fields
///   `search_id` (UUID), `tool` (`grep`|`glob`), and optionally `cwd` /
///   `search_ts`. No hook handoff — search self-scopes. (Field emission is
///   wired daemon-side at the firehose cutover, ticket 02; the resolver reads
///   the contract here.)
/// - **Server** — an autonomous LSP event (`kind = "lsp"`, names a `server`, no
///   session/search scope): `$/progress`, `window/logMessage`, spawn,
///   `initialize`. Triggered LSP rides its command's file via the session/search
///   scope above, not the server file.
/// - **Instance** — everything else: MCP heartbeat (`kind = "mcp"`) → the mcp
///   stream; internal trace → the trace stream. The `scope_id` is `instance_id`.
fn resolve_scope(event: &LogEvent<'_>, instance_id: &str) -> Scope {
    if let Some(session_id) = non_empty(event.session_id.as_deref()) {
        return Scope::Session {
            cwd: non_empty(event.scope_root.as_deref()),
            session_id,
        };
    }

    if let Some(scope) = search_scope(event) {
        return scope;
    }

    if event.kind.as_deref() == Some("lsp")
        && let Some(server) = non_empty(event.server.as_deref())
    {
        return Scope::Server {
            server,
            scope_root: non_empty(event.scope_root.as_deref()),
        };
    }

    let stream = if event.kind.as_deref() == Some("mcp") {
        InstanceStream::Mcp
    } else {
        InstanceStream::Trace
    };
    Scope::Instance {
        id: instance_id.to_string(),
        stream,
    }
}

/// Detect a search invocation from the daemon's `search_id`/`tool` tags.
///
/// Returns `None` (falling through to the server/instance branches) unless the
/// event carries a parseable `search_id` UUID and a recognized `tool`.
fn search_scope(event: &LogEvent<'_>) -> Option<Scope> {
    let id = Uuid::parse_str(event.fields.get("search_id")?.as_str()?).ok()?;
    let tool = match event.fields.get("tool").and_then(Value::as_str)? {
        "grep" => SearchTool::Grep,
        "glob" => SearchTool::Glob,
        _ => return None,
    };
    let cwd = non_empty(event.fields.get("cwd").and_then(Value::as_str))
        .or_else(|| non_empty(event.scope_root.as_deref()));
    let ts = event
        .fields
        .get("search_ts")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(Scope::Search { cwd, tool, ts, id })
}

/// `Some(owned)` when `s` is present and non-empty, else `None`.
fn non_empty(s: Option<&str>) -> Option<String> {
    s.filter(|v| !v.is_empty()).map(ToString::to_string)
}

/// Build the firehose root path under `<base>/catenary/<encoded-cwd>` (or the
/// bare root when no cwd is known).
fn cwd_base(root: &Path, cwd: Option<&str>) -> PathBuf {
    cwd.map_or_else(
        || root.to_path_buf(),
        |c| root.join(encode_cwd(Path::new(c))),
    )
}

/// Map a [`Severity`] to its firehose level tag.
const fn level_str(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warn => "warn",
        Severity::Info => "info",
        Severity::Debug => "debug",
    }
}

/// One firehose line — a [`serde::Serialize`] view over a [`LogEvent`] plus its
/// resolved `scope_id`. Keys are omitted when empty so lines stay compact.
///
/// Replaces the old `messages` row shape and `build_trace_payload`: protocol
/// `payload` nests as real JSON; internal events carry `message`/`language`/
/// `fields` as top-level keys; the stringified-blob and the `client` column are
/// gone.
#[derive(Debug, Serialize)]
struct Record<'a> {
    /// RFC3339 millis, UTC.
    ts: String,
    /// `lsp` | `mcp` | `hook` | `internal`.
    kind: &'a str,
    /// `error` | `warn` | `info` | `debug`.
    level: &'a str,
    /// Self-describing scope id (session id / search UUID / `server@root` /
    /// instance id). The file path is the authoritative selector.
    #[serde(skip_serializing_if = "str::is_empty")]
    scope_id: &'a str,
    /// Groups a request/response pair and a sub-scope/causation chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "str::is_empty")]
    server: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    scope_root: &'a str,
    /// Project shard + free `query` filter dimension.
    #[serde(skip_serializing_if = "str::is_empty")]
    cwd: &'a str,
    /// Protocol method, or module target (internal events).
    #[serde(skip_serializing_if = "str::is_empty")]
    method: &'a str,
    /// Subsystem taxonomy (mainly internal).
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a str>,
    /// Nested protocol JSON (protocol events only).
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
    /// Rendered message (internal events only).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    /// Language id (internal events only).
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    /// Remaining structured fields (internal events only).
    #[serde(skip_serializing_if = "Map::is_empty")]
    fields: Map<String, Value>,
}

/// Build a [`Record`] from an event and its resolved `scope_id` / `cwd`.
///
/// Protocol events (`kind` in `{lsp, mcp, hook}`) nest their raw JSON `payload`
/// and use the protocol `method`; internal events carry top-level
/// `message`/`language`/`fields` and use the module `target` as `method`.
fn build_record<'a>(event: &'a LogEvent<'_>, scope_id: &'a str, cwd: &'a str) -> Record<'a> {
    let kind = match event.kind.as_deref() {
        Some("lsp") => "lsp",
        Some("mcp") => "mcp",
        Some("hook") => "hook",
        _ => "internal",
    };
    let is_protocol = kind != "internal";

    let method = if is_protocol {
        event.method.as_deref().unwrap_or("")
    } else {
        event.target
    };

    let payload = if is_protocol {
        event
            .payload
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(parse_payload)
    } else {
        None
    };

    let (message, language, fields) = if is_protocol {
        (None, None, Map::new())
    } else {
        (
            Some(event.message.as_str()),
            event.language.as_deref(),
            event.fields.clone(),
        )
    };

    Record {
        ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        kind,
        level: level_str(event.severity),
        scope_id,
        parent_id: event.parent_id.as_deref(),
        server: event.server.as_deref().unwrap_or(""),
        scope_root: event.scope_root.as_deref().unwrap_or(""),
        cwd,
        method,
        source: event.source.as_deref(),
        payload,
        message,
        language,
        fields,
    }
}

/// Parse a protocol event's payload string into nested JSON. A payload that is
/// not valid JSON (should not happen for protocol events, which carry
/// serialized JSON-RPC) is preserved verbatim as a JSON string rather than
/// dropped.
fn parse_payload(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// A tiny bounded LRU over open append [`File`] handles.
///
/// Single-writer (the daemon), so the only synchronization is the enclosing
/// [`Mutex`]; appends are torn-write-safe because each line is written in one
/// `write_all` to an `O_APPEND` handle (readers skip an unterminated tail).
#[derive(Default)]
struct HandleCache {
    files: HashMap<PathBuf, File>,
    /// Recency order; front = least recently used.
    order: VecDeque<PathBuf>,
}

impl HandleCache {
    /// Append `bytes` to the file at `path`, opening (and creating parent dirs
    /// for) it on first use. On any IO error the (possibly stale) handle is
    /// dropped so the next append reopens.
    fn append(&mut self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let res = self.append_inner(path, bytes);
        if res.is_err() {
            self.files.remove(path);
        }
        res
    }

    fn append_inner(&mut self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if !self.files.contains_key(path) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            self.files.insert(path.to_path_buf(), file);
        }
        self.touch(path);
        self.evict_if_needed();
        // Present by construction (just inserted or already there). Degrade to a
        // no-op rather than panic in the unreachable miss.
        self.files
            .get_mut(path)
            .map_or(Ok(()), |file| file.write_all(bytes))
    }

    /// Move `path` to the most-recently-used end of the recency order.
    fn touch(&mut self, path: &Path) {
        if let Some(pos) = self.order.iter().position(|p| p == path) {
            self.order.remove(pos);
        }
        self.order.push_back(path.to_path_buf());
    }

    /// Close least-recently-used handles until at or under the cap. The
    /// just-touched handle is at the back, so it is never the victim.
    fn evict_if_needed(&mut self) {
        while self.files.len() > HANDLE_CACHE_CAP {
            match self.order.pop_front() {
                Some(victim) => {
                    self.files.remove(&victim);
                }
                None => break,
            }
        }
    }
}

/// Append-only JSONL firehose sink. Writes one record per line to the file
/// selected by the record's resolved scope.
///
/// **Additive:** runs alongside `MessageDbSink` until the firehose cutover
/// (ticket 02) wires it into the active sink set and retires the DB writes.
pub struct JsonlSink {
    /// Firehose root: `<cache_dir>/catenary/<instance_id>`.
    root: PathBuf,
    /// Daemon instance id — the `scope_id` for instance-global streams.
    instance_id: Arc<str>,
    /// LRU of open append handles, keyed by absolute file path.
    handles: Mutex<HandleCache>,
}

impl JsonlSink {
    /// Create a sink rooted at `<cache_dir>/catenary/<instance_id>`.
    ///
    /// `cache_dir` is the resolved cache base (production: [`crate::db::cache_dir`]);
    /// tests inject a tempdir so the firehose never escapes into `~/.cache`.
    #[must_use]
    pub fn new(cache_dir: &Path, instance_id: Arc<str>) -> Arc<Self> {
        let root = cache_dir.join("catenary").join(instance_id.as_ref());
        Arc::new(Self {
            root,
            instance_id,
            handles: Mutex::new(HandleCache::default()),
        })
    }
}

impl Sink for JsonlSink {
    fn handle(&self, event: &LogEvent<'_>) {
        let scope = resolve_scope(event, &self.instance_id);
        let (path, scope_id) = scope.target(&self.root);
        let record = build_record(event, &scope_id, scope.cwd());

        let mut line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => {
                // trace!, not warn!, to avoid a re-entrant event storm.
                tracing::trace!(error = %e, "jsonl_sink: serialize failed");
                return;
            }
        };
        line.push('\n');

        let result = {
            let mut guard = lock_handles(&self.handles);
            guard.append(&path, line.as_bytes())
        };
        if let Err(e) = result {
            tracing::trace!(error = %e, path = %path.display(), "jsonl_sink: append failed");
        }
    }
}

/// Recover a poisoned handle-cache lock by taking the inner guard, so logging
/// keeps working after an unrelated panic.
fn lock_handles(m: &Mutex<HandleCache>) -> std::sync::MutexGuard<'_, HandleCache> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use std::fs;
    use std::path::Path;

    use serde_json::Value;
    use uuid::Uuid;

    use super::InstanceStream;
    use super::JsonlSink;
    use super::Scope;
    use super::SearchTool;
    use super::build_record;
    use super::resolve_scope;
    use crate::logging::LogEvent;
    use crate::logging::Severity;
    use crate::logging::Sink;

    const INSTANCE: &str = "inst-abc";

    /// A bare event with all fields empty (an internal trace event).
    fn blank_event() -> LogEvent<'static> {
        LogEvent {
            severity: Severity::Info,
            target: "crate::module",
            message: String::new(),
            kind: None,
            method: None,
            server: None,
            client: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            scope_root: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    fn root() -> std::path::PathBuf {
        Path::new("/cache/catenary").join(INSTANCE)
    }

    // ── Record serialization ─────────────────────────────────────────

    #[test]
    fn protocol_record_nests_payload_no_client_no_message() {
        let mut e = blank_event();
        e.kind = Some("lsp".into());
        e.method = Some("textDocument/publishDiagnostics".into());
        e.server = Some("rust-analyzer".into());
        e.payload = Some(r#"{"uri":"file:///a.rs","diagnostics":[]}"#.into());

        let rec = build_record(&e, "rust-analyzer@/p", "/p");
        let v = serde_json::to_value(&rec).expect("serialize");

        assert_eq!(v["kind"], "lsp");
        assert_eq!(v["method"], "textDocument/publishDiagnostics");
        // payload nests as real JSON, not a stringified blob.
        assert!(v["payload"].is_object(), "payload should nest: {v}");
        assert_eq!(v["payload"]["uri"], "file:///a.rs");
        // No `client`, and internal-only keys absent for protocol events.
        assert!(v.get("client").is_none(), "client dropped: {v}");
        assert!(v.get("message").is_none(), "no message on protocol: {v}");
        assert!(v.get("fields").is_none(), "no fields on protocol: {v}");
    }

    #[test]
    fn internal_record_has_top_level_source_message_fields() {
        let mut e = blank_event();
        e.severity = Severity::Warn;
        e.message = "rust-analyzer exited".into();
        e.source = Some("lsp.lifecycle".into());
        e.language = Some("rust".into());
        e.fields.insert("code".into(), Value::Number(101.into()));

        let rec = build_record(&e, INSTANCE, "");
        let v = serde_json::to_value(&rec).expect("serialize");

        assert_eq!(v["kind"], "internal");
        assert_eq!(v["level"], "warn");
        // method falls back to the module target for internal events.
        assert_eq!(v["method"], "crate::module");
        assert_eq!(v["message"], "rust-analyzer exited");
        assert_eq!(v["source"], "lsp.lifecycle");
        assert_eq!(v["language"], "rust");
        assert_eq!(v["fields"]["code"], 101);
        // No nested protocol payload.
        assert!(v.get("payload").is_none(), "no payload on internal: {v}");
    }

    #[test]
    fn empty_keys_are_omitted() {
        let e = blank_event();
        let rec = build_record(&e, "", "");
        let v = serde_json::to_value(&rec).expect("serialize");
        for key in ["scope_id", "parent_id", "server", "scope_root", "cwd"] {
            assert!(v.get(key).is_none(), "{key} should be omitted: {v}");
        }
    }

    // ── Scope resolver → file path + scope_id ─────────────────────────

    #[test]
    fn session_scope_resolves_under_cwd() {
        let mut e = blank_event();
        e.session_id = Some("mcp:7f3a".into());
        e.scope_root = Some("/home/mark/Projects/Catenary".into());

        let scope = resolve_scope(&e, INSTANCE);
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, "mcp:7f3a");
        assert_eq!(
            path,
            root()
                .join("-home-mark-Projects-Catenary")
                .join("sessions")
                .join("mcp:7f3a.jsonl")
        );
    }

    #[test]
    fn session_scope_without_cwd_falls_back_to_root() {
        let mut e = blank_event();
        e.session_id = Some("s1".into());

        let scope = resolve_scope(&e, INSTANCE);
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, "s1");
        assert_eq!(path, root().join("sessions").join("s1.jsonl"));
    }

    #[test]
    fn search_scope_resolves_per_invocation_file() {
        let id = Uuid::parse_str("00000000-0000-4000-8000-000000000001").expect("uuid");
        let mut e = blank_event();
        e.fields
            .insert("search_id".into(), Value::String(id.to_string()));
        e.fields.insert("tool".into(), Value::String("grep".into()));
        e.fields.insert("cwd".into(), Value::String("/p".into()));
        e.fields.insert(
            "search_ts".into(),
            Value::String("20260608T143210123Z".into()),
        );

        let scope = resolve_scope(&e, INSTANCE);
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, id.simple().to_string());
        assert_eq!(
            path,
            root()
                .join("-p")
                .join("grep")
                .join(format!("20260608T143210123Z_{}.jsonl", id.simple()))
        );
    }

    #[test]
    fn search_scope_glob_tool_selects_glob_dir() {
        let mut e = blank_event();
        e.fields.insert(
            "search_id".into(),
            Value::String("00000000-0000-4000-8000-000000000002".into()),
        );
        e.fields.insert("tool".into(), Value::String("glob".into()));
        e.scope_root = Some("/p".into());

        let scope = resolve_scope(&e, INSTANCE);
        assert!(matches!(
            scope,
            Scope::Search {
                tool: SearchTool::Glob,
                ..
            }
        ));
        let (path, _) = scope.target(&root());
        // No search_ts → bare `<uuid>.jsonl`; cwd falls back to scope_root.
        assert!(path.starts_with(root().join("-p").join("glob")));
    }

    #[test]
    fn autonomous_server_rootful_resolves_under_cwd() {
        let mut e = blank_event();
        e.kind = Some("lsp".into());
        e.server = Some("rust-analyzer".into());
        e.method = Some("$/progress".into());
        e.scope_root = Some("/home/mark/Projects/Catenary".into());

        let scope = resolve_scope(&e, INSTANCE);
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, "rust-analyzer@/home/mark/Projects/Catenary");
        assert_eq!(
            path,
            root()
                .join("-home-mark-Projects-Catenary")
                .join("rust-analyzer.jsonl")
        );
    }

    #[test]
    fn autonomous_server_rootless_resolves_under_servers() {
        let mut e = blank_event();
        e.kind = Some("lsp".into());
        e.server = Some("taplo".into());

        let scope = resolve_scope(&e, INSTANCE);
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, "taplo");
        assert_eq!(path, root().join("servers").join("taplo.jsonl"));
    }

    #[test]
    fn mcp_event_resolves_to_instance_mcp_stream() {
        let mut e = blank_event();
        e.kind = Some("mcp".into());

        let scope = resolve_scope(&e, INSTANCE);
        assert!(matches!(
            scope,
            Scope::Instance {
                stream: InstanceStream::Mcp,
                ..
            }
        ));
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, INSTANCE);
        assert_eq!(path, root().join("mcp.jsonl"));
    }

    #[test]
    fn internal_event_resolves_to_instance_trace_stream() {
        let scope = resolve_scope(&blank_event(), INSTANCE);
        assert!(matches!(
            scope,
            Scope::Instance {
                stream: InstanceStream::Trace,
                ..
            }
        ));
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, INSTANCE);
        assert_eq!(path, root().join("trace.jsonl"));
    }

    #[test]
    fn session_id_takes_priority_over_server() {
        // A command-triggered LSP event carries a session_id and rides the
        // session file, not the server file.
        let mut e = blank_event();
        e.kind = Some("lsp".into());
        e.server = Some("rust-analyzer".into());
        e.session_id = Some("s9".into());
        e.scope_root = Some("/p".into());

        let scope = resolve_scope(&e, INSTANCE);
        assert!(matches!(scope, Scope::Session { .. }));
    }

    // ── Append behavior ──────────────────────────────────────────────

    #[test]
    fn two_events_same_scope_append_two_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = JsonlSink::new(dir.path(), INSTANCE.into());

        let mut e = blank_event();
        e.session_id = Some("s1".into());
        e.scope_root = Some("/p".into());
        e.message = "first".into();
        sink.handle(&e);
        e.message = "second".into();
        sink.handle(&e);

        let file = dir
            .path()
            .join("catenary")
            .join(INSTANCE)
            .join("-p")
            .join("sessions")
            .join("s1.jsonl");
        let contents = fs::read_to_string(&file).expect("read jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected two lines: {contents:?}");

        let first: Value = serde_json::from_str(lines[0]).expect("parse line 0");
        let second: Value = serde_json::from_str(lines[1]).expect("parse line 1");
        assert_eq!(first["message"], "first");
        assert_eq!(second["message"], "second");
        assert_eq!(first["scope_id"], "s1");
    }

    #[test]
    fn distinct_scopes_write_distinct_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = JsonlSink::new(dir.path(), INSTANCE.into());

        let mut session = blank_event();
        session.session_id = Some("s1".into());
        sink.handle(&session);

        let mut server = blank_event();
        server.kind = Some("lsp".into());
        server.server = Some("taplo".into());
        sink.handle(&server);

        let base = dir.path().join("catenary").join(INSTANCE);
        assert!(base.join("sessions").join("s1.jsonl").exists());
        assert!(base.join("servers").join("taplo.jsonl").exists());
    }

    #[test]
    fn firehose_stays_under_injected_root() {
        // The isolation guarantee: every write lands under the sink's root, so
        // with an isolated cache base (tempdir / XDG_CACHE_HOME) nothing
        // escapes into the real `~/.cache`.
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = JsonlSink::new(dir.path(), INSTANCE.into());

        let mut e = blank_event();
        e.kind = Some("mcp".into());
        sink.handle(&e);
        let mut e2 = blank_event();
        e2.message = "trace".into();
        sink.handle(&e2);

        // Both instance streams exist, both under the tempdir.
        let base = dir.path().join("catenary").join(INSTANCE);
        assert!(base.join("mcp.jsonl").exists());
        assert!(base.join("trace.jsonl").exists());
    }

    #[test]
    fn db_failure_style_unparseable_payload_preserved_as_string() {
        let mut e = blank_event();
        e.kind = Some("lsp".into());
        e.payload = Some("not json".into());
        let rec = build_record(&e, "sid", "");
        let v = serde_json::to_value(&rec).expect("serialize");
        assert_eq!(v["payload"], "not json");
    }
}
