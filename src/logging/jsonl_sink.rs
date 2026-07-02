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
//! ├── mcp.jsonl                          # instance-global MCP heartbeat
//! ├── trace.jsonl                        # instance-global internal trace (level-filtered)
//! ├── servers/<server>@<enc-root>.jsonl  # autonomous server lifecycle, sharded by server identity
//! ├── grep/<ts>_<uuid>.jsonl             # one invocation = one file (cmd record + triggered LSP)
//! ├── glob/<ts>_<uuid>.jsonl
//! └── sessions/<session_id>.jsonl        # hook decisions · edits · diagnostics(+LSP)
//! ```
//!
//! **Each scope shards by its own id at the top level**; `cwd` is a *record
//! field* (a `query` filter dimension), not a directory layer. Servers shard by
//! their `server@root` identity. (Earlier drafts nested everything under an
//! `<encoded-cwd>/` project dir — dropped: it split a session's records across
//! files and made `query --session` a cross-dir scan instead of a direct open.
//! A project subtree can be re-added later without touching the record format,
//! since `cwd` is in every line.)
//!
//! **The live firehose sink.** Wired into the [`LoggingServer`](super) active
//! sink set in place of the retired `MessageDbSink` (firehose cutover, ticket
//! 02). The write path is decoupled from the emitting thread: `handle` does only
//! cheap work (resolve the scope, build the record, serialize) and enqueues onto
//! a bounded channel; a single dedicated writer thread owns the file handles and
//! drains the channel, so disk latency never couples to LSP/MCP dispatch. A
//! saturated channel drops (counted, surfaced) rather than blocking — the daemon
//! is structurally unwedgeable on the writer side too, not just the readers.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;

use chrono::SecondsFormat;
use chrono::Utc;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use uuid::Uuid;

use super::LogEvent;
use super::Severity;
use super::Sink;
use super::reaper::ReapPolicy;
use crate::paths::encode_cwd;

/// Max number of open append file handles kept warm.
///
/// Reopening a file per line would dominate the write cost; this keeps the hot
/// files (the active sessions/servers/streams) open. Beyond the cap, the
/// least-recently-used handle is closed. Sized generously — a busy daemon has a
/// handful of live scopes, not dozens.
const HANDLE_CACHE_CAP: usize = 64;

/// Bounded capacity of the write queue between [`JsonlSink::handle`] and the
/// dedicated writer thread.
///
/// `handle` runs on the thread that emitted the tracing event (an LSP-dispatch
/// thread, the MCP reader, …); a synchronous file write there would couple disk
/// latency to dispatch — the residual wedge vector once size-driven growth is
/// gone. So `handle` only enqueues and the writer thread owns the I/O. When the
/// queue is full (writer stalled on slow/blocked storage) sends are **dropped,
/// never blocked** — telemetry is regenerable; blocking dispatch is the failure
/// being eliminated. Sized for bursty LSP traffic after the emission-side level
/// filter; at a few hundred bytes per line the cap bounds the queue well under
/// ~8 MB.
const CHANNEL_CAP: usize = 8192;

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

/// How the write path bounds the file a record lands in (ticket 01).
///
/// Each [`Scope`] maps to exactly one of these, so the writer applies the right
/// on-write reaper after appending: streams roll into bounded segments, tool
/// dirs evict oldest call files under a byte budget, and session files are left
/// unbounded (append is O(1); the periodic staleness sweep is their only reaper).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReapClass {
    /// A never-idle stream (`mcp`/`trace`/`servers`): segment rotation.
    Stream,
    /// A per-tool dir (`grep`/`glob`): byte-budget eviction of oldest call files.
    ToolDir,
    /// No size bound (session files).
    Unbounded,
}

/// The top-level scope a record belongs to.
///
/// Each variant maps (via [`Scope::target`]) to exactly one JSONL file under the
/// firehose root plus a self-describing `scope_id` string. The file path is the
/// authoritative selector; `scope_id` is only for self-description when a record
/// is pulled out of its file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// Hook-correlated work (editing, `diagnostics`), keyed by the
    /// session id. Lives at `sessions/<session_id>.jsonl` — `session_id` is the
    /// primary forensic axis, so it shards at the top level (a direct open for
    /// `query --session`, not a cross-dir scan).
    Session { session_id: String },
    /// One stateless `grep`/`glob` invocation, keyed by a daemon-minted
    /// per-request UUID (no hook handoff — Decision 4). Lives at
    /// `<tool>/<ts>_<uuid>.jsonl`.
    Search {
        tool: SearchTool,
        ts: String,
        id: Uuid,
    },
    /// A server's *autonomous* lifecycle (`$/progress`, `window/logMessage`,
    /// spawn, `initialize`) — traffic no request triggered. Sharded by the
    /// server's own `server@root` identity: rootful → `servers/<server>@<enc-root>.jsonl`,
    /// rootless (workspace/single-file tier) → `servers/<server>.jsonl`.
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
            Self::Session { session_id } => (
                root.join("sessions").join(format!("{session_id}.jsonl")),
                session_id.clone(),
            ),
            Self::Search { tool, ts, id } => {
                let hex = id.simple().to_string();
                let name = if ts.is_empty() {
                    format!("{hex}.jsonl")
                } else {
                    format!("{ts}_{hex}.jsonl")
                };
                (root.join(tool.dir_name()).join(name), hex)
            }
            Self::Server { server, scope_root } => {
                let servers = root.join("servers");
                scope_root.as_deref().map_or_else(
                    || (servers.join(format!("{server}.jsonl")), server.clone()),
                    |r| {
                        let enc = encode_cwd(Path::new(r));
                        (
                            servers.join(format!("{server}@{enc}.jsonl")),
                            format!("{server}@{r}"),
                        )
                    },
                )
            }
            Self::Instance { id, stream } => (root.join(stream.file_name()), id.clone()),
        }
    }

    /// Which on-write reaper bounds this scope's file (ticket 01).
    const fn reap_class(&self) -> ReapClass {
        match self {
            Self::Session { .. } => ReapClass::Unbounded,
            Self::Search { .. } => ReapClass::ToolDir,
            Self::Server { .. } | Self::Instance { .. } => ReapClass::Stream,
        }
    }
}

/// Resolve the scope of an event, daemon-side, at write time.
///
/// Generalizes the degenerate selector the retired DB sink used
/// (`session_id.unwrap_or(instance_id)`) into the four-way
/// `session → search → server → instance` rule:
///
/// - **Session** — the event carries a `session_id` (hook-correlated work, plus
///   any LSP it triggered, which inherits the session span).
/// - **Search** — a stateless `grep`/`glob` invocation. The daemon mints a
///   per-request UUID at the IPC boundary and tags the invocation's events (the
///   command record and any LSP it triggers) with the structured fields
///   `search_id` (UUID), `tool` (`grep`|`glob`), and optionally `search_ts`. No
///   hook handoff — search self-scopes. (Field emission is wired daemon-side
///   where the daemon mints the per-request id; the resolver reads the contract
///   here.)
/// - **Server** — an autonomous LSP event (`kind = "lsp"`, names a `server`, no
///   session/search scope): `$/progress`, `window/logMessage`, spawn,
///   `initialize`. Triggered LSP rides its command's file via the session/search
///   scope above, not the server file.
/// - **Instance** — everything else: MCP heartbeat (`kind = "mcp"`) → the mcp
///   stream; internal trace → the trace stream. The `scope_id` is `instance_id`.
fn resolve_scope(event: &LogEvent<'_>, instance_id: &str) -> Scope {
    if let Some(session_id) = non_empty(event.session_id.as_deref()) {
        return Scope::Session { session_id };
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
    let ts = event
        .fields
        .get("search_ts")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(Scope::Search { tool, ts, id })
}

/// `Some(owned)` when `s` is present and non-empty, else `None`.
fn non_empty(s: Option<&str>) -> Option<String> {
    s.filter(|v| !v.is_empty()).map(ToString::to_string)
}

/// The cwd shard string for the record body — the project a record belongs to,
/// as a `query` filter dimension (not a directory level).
///
/// Prefers the search contract's explicit `cwd` field (the CLI-side `getcwd`),
/// falling back to the LSP routing `scope_root`. Empty when neither is present
/// (e.g. instance-global streams). When a session does not carry an explicit
/// CLI-side `getcwd`, `scope_root` is the proxy.
fn event_cwd<'a>(event: &'a LogEvent<'_>) -> &'a str {
    event
        .fields
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .or(event.scope_root.as_deref())
        .unwrap_or("")
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

/// Build a [`Record`] from an event and its resolved `scope_id`.
///
/// Protocol events (`kind` in `{lsp, mcp, hook}`) nest their raw JSON `payload`
/// and use the protocol `method`; internal events carry top-level
/// `message`/`language`/`fields` and use the module `target` as `method`. The
/// `cwd` filter field is derived from the event via [`event_cwd`].
fn build_record<'a>(event: &'a LogEvent<'_>, scope_id: &'a str) -> Record<'a> {
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
        cwd: event_cwd(event),
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

/// One open append handle plus a running byte counter for rotation.
///
/// `bytes` is seeded from the file's on-disk length when (re)opened — so a
/// handle evicted from the LRU and reopened mid-life resumes the correct count —
/// and incremented per append. No `stat` per line.
struct FileSlot {
    file: File,
    bytes: u64,
}

/// A tiny bounded LRU over open append [`File`] handles, plus the on-write
/// reapers (rotation + per-tool byte budget, ticket 01).
///
/// Owned exclusively by the single writer thread, so it needs no
/// synchronization. Appends are torn-write-safe because each line is written in
/// one `write_all` to an `O_APPEND` handle (readers skip an unterminated tail).
#[derive(Default)]
struct HandleCache {
    files: HashMap<PathBuf, FileSlot>,
    /// Recency order; front = least recently used.
    order: VecDeque<PathBuf>,
    /// Running byte total per tool dir (`grep`/`glob`), for budget eviction.
    /// Seeded lazily and reset to the authoritative scan total after each evict.
    tool_dir_bytes: HashMap<PathBuf, u64>,
}

impl HandleCache {
    /// Append `bytes` to the file at `path`, opening (and creating parent dirs
    /// for) it on first use, then apply `class`'s on-write reaper. On any IO
    /// error the (possibly stale) handle is dropped so the next append reopens.
    fn append(
        &mut self,
        path: &Path,
        class: ReapClass,
        bytes: &[u8],
        policy: &ReapPolicy,
    ) -> std::io::Result<()> {
        let res = self.append_inner(path, bytes);
        if res.is_err() {
            self.files.remove(path);
            return res;
        }
        // Post-write reaping on the just-written file's class.
        match class {
            ReapClass::Stream => self.rotate_if_needed(path, policy),
            ReapClass::ToolDir => self.enforce_tool_budget(path, bytes.len() as u64, policy),
            ReapClass::Unbounded => {}
        }
        res
    }

    fn append_inner(&mut self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if !self.files.contains_key(path) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            let existing = file.metadata().map_or(0, |m| m.len());
            self.files.insert(
                path.to_path_buf(),
                FileSlot {
                    file,
                    bytes: existing,
                },
            );
        }
        self.touch(path);
        self.evict_if_needed();
        // Present by construction (just inserted or already there). Degrade to a
        // no-op rather than panic in the unreachable miss.
        let Some(slot) = self.files.get_mut(path) else {
            return Ok(());
        };
        slot.file.write_all(bytes)?;
        slot.bytes += bytes.len() as u64;
        Ok(())
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

    /// Drop a cached handle and its recency slot (the file was rolled or evicted
    /// from disk).
    fn forget(&mut self, path: &Path) {
        self.files.remove(path);
        if let Some(pos) = self.order.iter().position(|p| p == path) {
            self.order.remove(pos);
        }
    }

    /// Roll a stream's live segment once it reaches `segment_bytes`: close the
    /// handle and shift `name.jsonl → name.1.jsonl → … → name.{K-1}.jsonl`,
    /// dropping the oldest. The next append reopens a fresh live file.
    fn rotate_if_needed(&mut self, path: &Path, policy: &ReapPolicy) {
        let over = self
            .files
            .get(path)
            .is_some_and(|s| s.bytes >= policy.segment_bytes);
        if !over {
            return;
        }
        self.forget(path);
        roll_segments(path, policy.segments_kept);
    }

    /// Keep a tool dir (`grep`/`glob`) under its byte budget by evicting the
    /// oldest call files (lexical ts-prefix order). The incremental counter
    /// decides *when* to scan; eviction itself is authoritative (a real
    /// `readdir`), and resets the counter to the post-evict total.
    fn enforce_tool_budget(&mut self, path: &Path, added: u64, policy: &ReapPolicy) {
        let Some(dir) = path.parent() else {
            return;
        };
        let total = self.tool_dir_bytes.entry(dir.to_path_buf()).or_insert(0);
        *total += added;
        if *total <= policy.tool_dir_budget {
            return;
        }
        let remaining = self.evict_tool_dir(dir, path, policy.tool_dir_budget);
        self.tool_dir_bytes.insert(dir.to_path_buf(), remaining);
    }

    /// Delete oldest-by-name files in `dir` until its total is under `budget`,
    /// never evicting `active` (the file just written). Returns the post-evict
    /// total. Drops cached handles for removed files.
    fn evict_tool_dir(&mut self, dir: &Path, active: &Path, budget: u64) -> u64 {
        let Ok(read) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut files: Vec<(PathBuf, u64)> = read
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let len = e.metadata().ok().filter(std::fs::Metadata::is_file)?.len();
                Some((p, len))
            })
            .collect();
        // Filenames are ts-prefixed, so lexical order is chronological.
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut total: u64 = files.iter().map(|(_, len)| *len).sum();
        for (p, len) in files {
            if total <= budget {
                break;
            }
            if p == active {
                continue; // never evict the file we just wrote
            }
            if std::fs::remove_file(&p).is_ok() {
                total = total.saturating_sub(len);
                self.forget(&p);
            }
        }
        total
    }
}

/// Roll a stream's segments: `name.{K-1}.jsonl` is dropped, each `name.{i}.jsonl`
/// shifts to `name.{i+1}.jsonl`, and the live `name.jsonl` becomes `name.1.jsonl`.
///
/// `keep` (`K`) is the total segments retained **including the live file**, so
/// rotated segments number `K-1`. `K ≤ 1` keeps no history — the live file is
/// simply truncated (removed) on roll. Missing intermediate segments are
/// tolerated (early rolls). Best-effort: rename/remove errors are ignored.
fn roll_segments(path: &Path, keep: usize) {
    let rotated = keep.saturating_sub(1);
    if rotated == 0 {
        let _ = std::fs::remove_file(path);
        return;
    }
    // Drop the oldest rotated segment, then shift the rest down by one.
    let _ = std::fs::remove_file(segment_path(path, rotated));
    for i in (1..rotated).rev() {
        let from = segment_path(path, i);
        if from.exists() {
            let _ = std::fs::rename(&from, segment_path(path, i + 1));
        }
    }
    let _ = std::fs::rename(path, segment_path(path, 1));
}

/// The rotated-segment path for index `n`: `…/name.jsonl` → `…/name.{n}.jsonl`.
fn segment_path(path: &Path, n: usize) -> PathBuf {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let base = name.strip_suffix(".jsonl").unwrap_or(name);
    path.with_file_name(format!("{base}.{n}.jsonl"))
}

/// Append-only JSONL firehose sink. Writes one record per line to the file
/// selected by the record's resolved scope.
///
/// The write path is decoupled from the emitting thread. [`Sink::handle`] runs
/// on whatever thread emitted the tracing event (an LSP-dispatch thread, the MCP
/// reader, …); it does only cheap work — resolve the scope, build the record,
/// serialize — then enqueues the `(path, line)` onto a bounded channel and
/// returns. A single dedicated writer thread drains the channel, owns the
/// [`HandleCache`], and batches one `write_all` per file per drain. A saturated
/// channel **drops** (counted, surfaced) rather than blocking dispatch — the one
/// residual wedge vector once size-driven growth is gone (a stalled NFS-mounted
/// cache dir or a full disk must not stall the daemon).
pub struct JsonlSink {
    /// Firehose root: `<cache_dir>/catenary/<instance_id>`.
    root: PathBuf,
    /// Daemon instance id — the `scope_id` for instance-global streams.
    instance_id: Arc<str>,
    /// Bounded write queue to the writer thread. The sink is the sole producer
    /// facade and is itself shared as `Arc<JsonlSink>`, so no sender cloning.
    tx: SyncSender<Job>,
    /// Writer thread handle, taken and joined by [`JsonlSink::shutdown`].
    /// `None` if the thread failed to spawn (degrades to drop-everything).
    writer: Mutex<Option<JoinHandle<()>>>,
    /// Count of lines dropped because the queue was full (or the writer is
    /// gone). Surfaced periodically by the writer; never silent.
    dropped: Arc<AtomicU64>,
}

/// A unit of work for the writer thread.
enum Job {
    /// Append `line` to the file at `path`, then apply `class`'s on-write reaper.
    Write {
        path: PathBuf,
        class: ReapClass,
        line: Vec<u8>,
    },
    /// Drain everything queued, then exit (clean-shutdown sentinel).
    Shutdown,
}

impl JsonlSink {
    /// Create a sink rooted at `<cache_dir>/catenary/<instance_id>` with the
    /// default [`ReapPolicy`], and spawn its writer thread.
    ///
    /// `cache_dir` is the resolved cache base (production: [`crate::paths::cache_dir`]);
    /// tests inject a tempdir so the firehose never escapes into `~/.cache`.
    #[must_use]
    pub fn new(cache_dir: &Path, instance_id: Arc<str>) -> Arc<Self> {
        Self::with_policy(cache_dir, instance_id, ReapPolicy::default())
    }

    /// Like [`Self::new`] with an explicit [`ReapPolicy`] for the on-write
    /// reapers (rotation + per-tool byte budget, ticket 01). The daemon passes
    /// the config-resolved policy; tests inject small knobs to exercise reaping.
    #[must_use]
    pub fn with_policy(cache_dir: &Path, instance_id: Arc<str>, policy: ReapPolicy) -> Arc<Self> {
        let root = cache_dir.join("catenary").join(instance_id.as_ref());
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(CHANNEL_CAP);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = Arc::clone(&dropped);
        let writer = std::thread::Builder::new()
            .name("catenary-jsonl".to_string())
            .spawn(move || writer_loop(&rx, &writer_dropped, policy))
            .ok();
        Arc::new(Self {
            root,
            instance_id,
            tx,
            writer: Mutex::new(writer),
            dropped,
        })
    }

    /// Drain any queued lines and stop the writer thread.
    ///
    /// Called on clean daemon shutdown. Sends the [`Job::Shutdown`] sentinel — a
    /// blocking send, because at shutdown we wait for the writer to make room and
    /// drain rather than drop — and joins the writer, so every line enqueued
    /// before this call is on disk when it returns. A crash skips this and may
    /// lose the unflushed tail (acceptable; the per-instance dir isolates a torn
    /// tail from the next daemon). Idempotent: a second call is a no-op.
    pub fn shutdown(&self) {
        // Err means the writer already exited (prior shutdown / spawn failure).
        let _ = self.tx.send(Job::Shutdown);
        // Take the handle out (releasing the lock) before joining, so the join
        // never happens under the mutex.
        let handle = lock_writer(&self.writer).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// Lines dropped so far under queue backpressure (test accessor).
    #[cfg(test)]
    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Sink for JsonlSink {
    fn handle(&self, event: &LogEvent<'_>) {
        let scope = resolve_scope(event, &self.instance_id);
        let class = scope.reap_class();
        let (path, scope_id) = scope.target(&self.root);
        let record = build_record(event, &scope_id);

        let mut line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => {
                // trace!, not warn!, to avoid a re-entrant event storm.
                tracing::trace!(error = %e, "jsonl_sink: serialize failed");
                return;
            }
        };
        line.push('\n');

        // Enqueue and return — never block the emitting (dispatch) thread. A
        // full queue (writer stalled) or a gone writer drops the line.
        if self
            .tx
            .try_send(Job::Write {
                path,
                class,
                line: line.into_bytes(),
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The writer thread: drain [`Job`]s, batching one `write_all` per file per
/// wakeup, until the [`Job::Shutdown`] sentinel (or all senders drop). Owns the
/// [`HandleCache`] and applies the on-write reapers per file via `policy`.
fn writer_loop(rx: &Receiver<Job>, dropped: &AtomicU64, policy: ReapPolicy) {
    let mut cache = HandleCache::default();
    let mut last_reported = 0u64;
    let mut stop = false;
    while !stop {
        // Block for at least one job, then absorb everything currently queued
        // into a per-file batch so each file takes a single `write_all`.
        let Ok(first) = rx.recv() else { break };
        let mut batch: HashMap<PathBuf, (ReapClass, Vec<u8>)> = HashMap::new();
        merge_job(&mut batch, first, &mut stop);
        while let Ok(job) = rx.try_recv() {
            merge_job(&mut batch, job, &mut stop);
        }
        for (path, (class, bytes)) in &batch {
            if let Err(e) = cache.append(path, *class, bytes, &policy) {
                tracing::trace!(error = %e, path = %path.display(), "jsonl_sink: append failed");
            }
        }
        report_drops(dropped, &mut last_reported);
    }
}

/// Fold one [`Job`] into the current drain batch. [`Job::Shutdown`] sets `stop`.
/// A path's [`ReapClass`] is invariant, so the first write fixes it.
fn merge_job(batch: &mut HashMap<PathBuf, (ReapClass, Vec<u8>)>, job: Job, stop: &mut bool) {
    match job {
        Job::Write { path, class, line } => batch
            .entry(path)
            .or_insert_with(|| (class, Vec::new()))
            .1
            .extend_from_slice(&line),
        Job::Shutdown => *stop = true,
    }
}

/// Surface newly-dropped lines once per drain cycle, so backpressure loss is
/// never silent. `debug!` (not `trace!`, which the level filter drops, and not
/// `warn!`, which would reach the user notification queue) lands it in
/// `trace.jsonl`.
fn report_drops(dropped: &AtomicU64, last_reported: &mut u64) {
    let total = dropped.load(Ordering::Relaxed);
    if total > *last_reported {
        let delta = total - *last_reported;
        *last_reported = total;
        tracing::debug!(
            source = crate::source::Source::LoggingFirehose.as_str(),
            dropped = total,
            "jsonl firehose dropped {delta} line(s) under backpressure (total {total})",
        );
    }
}

/// Recover a poisoned writer-handle lock so shutdown still joins the thread.
fn lock_writer(
    m: &Mutex<Option<JoinHandle<()>>>,
) -> std::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use std::fs;
    use std::path::Path;

    use serde_json::Value;
    use uuid::Uuid;

    use super::HandleCache;
    use super::InstanceStream;
    use super::JsonlSink;
    use super::ReapClass;
    use super::Scope;
    use super::SearchTool;
    use super::build_record;
    use super::resolve_scope;
    use super::segment_path;
    use crate::logging::LogEvent;
    use crate::logging::Severity;
    use crate::logging::Sink;
    use crate::logging::reaper::ReapPolicy;

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

        let rec = build_record(&e, "rust-analyzer@/p");
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

        let rec = build_record(&e, INSTANCE);
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
        let rec = build_record(&e, "");
        let v = serde_json::to_value(&rec).expect("serialize");
        for key in ["scope_id", "parent_id", "server", "scope_root", "cwd"] {
            assert!(v.get(key).is_none(), "{key} should be omitted: {v}");
        }
    }

    #[test]
    fn cwd_field_prefers_explicit_then_scope_root() {
        // Search contract: explicit `cwd` field is the CLI-side getcwd.
        let mut e = blank_event();
        e.scope_root = Some("/marker/root".into());
        e.fields
            .insert("cwd".into(), Value::String("/marker/root/src".into()));
        let v = serde_json::to_value(build_record(&e, "")).expect("serialize");
        assert_eq!(v["cwd"], "/marker/root/src");

        // Falls back to scope_root when no explicit cwd.
        let mut e2 = blank_event();
        e2.scope_root = Some("/marker/root".into());
        let v2 = serde_json::to_value(build_record(&e2, "")).expect("serialize");
        assert_eq!(v2["cwd"], "/marker/root");
    }

    // ── Scope resolver → file path + scope_id ─────────────────────────

    #[test]
    fn session_scope_resolves_at_top_level() {
        let mut e = blank_event();
        e.session_id = Some("mcp:7f3a".into());

        let scope = resolve_scope(&e, INSTANCE);
        let (path, scope_id) = scope.target(&root());
        assert_eq!(scope_id, "mcp:7f3a");
        assert_eq!(path, root().join("sessions").join("mcp:7f3a.jsonl"));
    }

    #[test]
    fn session_path_ignores_scope_root() {
        // cwd is a record field, not a directory level: scope_root must not
        // change where a session's file lives (else its records split).
        let mut bare = blank_event();
        bare.session_id = Some("s1".into());
        let mut rooted = blank_event();
        rooted.session_id = Some("s1".into());
        rooted.scope_root = Some("/home/mark/Projects/Catenary".into());

        let (bare_path, _) = resolve_scope(&bare, INSTANCE).target(&root());
        let (rooted_path, _) = resolve_scope(&rooted, INSTANCE).target(&root());
        assert_eq!(bare_path, rooted_path);
        assert_eq!(bare_path, root().join("sessions").join("s1.jsonl"));
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
                .join("grep")
                .join(format!("20260608T143210123Z_{}.jsonl", id.simple()))
        );
        // The search cwd rides as a record field, not in the path.
        assert_eq!(
            serde_json::to_value(build_record(&e, &scope_id)).expect("serialize")["cwd"],
            "/p"
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

        let scope = resolve_scope(&e, INSTANCE);
        assert!(matches!(
            scope,
            Scope::Search {
                tool: SearchTool::Glob,
                ..
            }
        ));
        let (path, _) = scope.target(&root());
        // No search_ts → bare `<uuid>.jsonl`, directly under the top-level glob dir.
        assert!(path.starts_with(root().join("glob")));
    }

    #[test]
    fn autonomous_server_rootful_resolves_under_servers() {
        let mut e = blank_event();
        e.kind = Some("lsp".into());
        e.server = Some("rust-analyzer".into());
        e.method = Some("$/progress".into());
        e.scope_root = Some("/home/mark/Projects/Catenary".into());

        let scope = resolve_scope(&e, INSTANCE);
        let (path, scope_id) = scope.target(&root());
        // scope_id carries the raw root (round-trips into `query --server`); the
        // filename encodes it (path-safe).
        assert_eq!(scope_id, "rust-analyzer@/home/mark/Projects/Catenary");
        assert_eq!(
            path,
            root()
                .join("servers")
                .join("rust-analyzer@-home-mark-Projects-Catenary.jsonl")
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
        // Flush the async writer before reading the file back.
        sink.shutdown();

        let file = dir
            .path()
            .join("catenary")
            .join(INSTANCE)
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
        sink.shutdown();

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
        sink.shutdown();

        // Both instance streams exist, both under the tempdir.
        let base = dir.path().join("catenary").join(INSTANCE);
        assert!(base.join("mcp.jsonl").exists());
        assert!(base.join("trace.jsonl").exists());
    }

    // ── Bounded write queue + flush-on-shutdown ──────────────────────

    #[test]
    fn shutdown_flushes_all_queued_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = JsonlSink::new(dir.path(), INSTANCE.into());

        let mut e = blank_event();
        e.session_id = Some("s1".into());
        for i in 0..100 {
            e.message = format!("line {i}");
            sink.handle(&e);
        }
        // shutdown() drains the queue and joins the writer: every enqueued line
        // is on disk afterward.
        sink.shutdown();

        let file = dir
            .path()
            .join("catenary")
            .join(INSTANCE)
            .join("sessions")
            .join("s1.jsonl");
        let contents = fs::read_to_string(&file).expect("read jsonl");
        assert_eq!(
            contents.lines().count(),
            100,
            "all queued lines flushed on shutdown"
        );
    }

    #[test]
    fn post_shutdown_sends_drop_and_count_without_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = JsonlSink::new(dir.path(), INSTANCE.into());
        // Stop the writer first: subsequent sends have nowhere to go and must be
        // dropped (counted), never block the emitting thread.
        sink.shutdown();

        let mut e = blank_event();
        e.session_id = Some("s1".into());
        for _ in 0..3 {
            sink.handle(&e);
        }
        assert_eq!(
            sink.dropped_count(),
            3,
            "post-shutdown sends are dropped and counted"
        );
    }

    #[test]
    fn db_failure_style_unparseable_payload_preserved_as_string() {
        let mut e = blank_event();
        e.kind = Some("lsp".into());
        e.payload = Some("not json".into());
        let rec = build_record(&e, "sid");
        let v = serde_json::to_value(&rec).expect("serialize");
        assert_eq!(v["payload"], "not json");
    }

    // ── On-write reaping: rotation + per-tool budget (ticket 01) ──────

    /// Count files in `dir` whose name starts with `prefix`.
    fn count_prefixed(dir: &Path, prefix: &str) -> usize {
        fs::read_dir(dir)
            .expect("read_dir")
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(prefix))
            })
            .count()
    }

    #[test]
    fn stream_rotation_keeps_k_segments_and_drops_oldest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trace.jsonl");
        // ~10 bytes/line, 50-byte segments → ~5 lines/segment; K = 3 total.
        let policy = ReapPolicy {
            segment_bytes: 50,
            segments_kept: 3,
            ..ReapPolicy::default()
        };
        // 38 lines at 10 bytes each → rolls every 5; ends mid-segment (lines
        // 36–38 in the live file), so the ceiling is exactly populated rather
        // than caught transiently empty just after a roll.
        let mut cache = HandleCache::default();
        for i in 0..38 {
            let line = format!("line-{i:04}\n");
            cache
                .append(&path, ReapClass::Stream, line.as_bytes(), &policy)
                .expect("append");
        }

        // Exactly K segments survive (live + 2 rotated); the rest rolled off.
        assert_eq!(
            count_prefixed(dir.path(), "trace"),
            3,
            "K=3 total segments retained"
        );
        // The live file holds the most recent lines.
        let live = fs::read_to_string(&path).expect("read live");
        assert!(
            live.contains("line-0037"),
            "newest line in the live segment: {live:?}"
        );
        // Oldest content is gone (line 0 cannot be in any of the 3 kept segments).
        let all: String = ["trace.jsonl", "trace.1.jsonl", "trace.2.jsonl"]
            .iter()
            .filter_map(|n| fs::read_to_string(dir.path().join(n)).ok())
            .collect();
        assert!(!all.contains("line-0000"), "oldest line rolled off");
    }

    #[test]
    fn unbounded_class_never_rotates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions").join("s1.jsonl");
        let policy = ReapPolicy {
            segment_bytes: 20,
            segments_kept: 3,
            ..ReapPolicy::default()
        };
        let mut cache = HandleCache::default();
        for i in 0..50 {
            let line = format!("line-{i:04}\n");
            cache
                .append(&path, ReapClass::Unbounded, line.as_bytes(), &policy)
                .expect("append");
        }
        // No segment files; session files are append-only and unbounded.
        assert_eq!(count_prefixed(&dir.path().join("sessions"), "s1"), 1);
        let body = fs::read_to_string(&path).expect("read");
        assert_eq!(body.lines().count(), 50, "every line retained");
    }

    #[test]
    fn tool_dir_budget_evicts_oldest_by_ts_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let grep = dir.path().join("grep");
        let policy = ReapPolicy {
            tool_dir_budget: 100,
            ..ReapPolicy::default()
        };
        let mut cache = HandleCache::default();
        // Ten ts-prefixed call files, 30 bytes each → only the newest few fit.
        let blob = vec![b'x'; 30];
        for i in 0..10 {
            let p = grep.join(format!("2026060812000{i}_abc.jsonl"));
            cache
                .append(&p, ReapClass::ToolDir, &blob, &policy)
                .expect("append");
        }

        // Dir stays under budget (modulo the never-evicted active file).
        let total: u64 = fs::read_dir(&grep)
            .expect("read_dir")
            .flatten()
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .sum();
        assert!(total <= 100, "dir kept under budget: {total}");
        // Oldest evicted, newest kept (eviction is lexical = chronological).
        assert!(
            !grep.join("20260608120000_abc.jsonl").exists(),
            "oldest call file evicted"
        );
        assert!(
            grep.join("20260608120009_abc.jsonl").exists(),
            "newest call file kept"
        );
    }

    #[test]
    fn tool_dir_keeps_single_oversized_active_file() {
        // A single invocation file larger than the budget is tolerated — the
        // active file is never the eviction victim.
        let dir = tempfile::tempdir().expect("tempdir");
        let grep = dir.path().join("grep");
        let policy = ReapPolicy {
            tool_dir_budget: 100,
            ..ReapPolicy::default()
        };
        let mut cache = HandleCache::default();
        let p = grep.join("20260608120000_big.jsonl");
        cache
            .append(&p, ReapClass::ToolDir, &vec![b'x'; 250], &policy)
            .expect("append");
        assert!(p.exists(), "oversized active file is not self-evicted");
    }

    #[test]
    fn segment_path_inserts_index_before_jsonl() {
        let p = Path::new("/c/servers/rust-analyzer@-p-Catenary.jsonl");
        assert_eq!(
            segment_path(p, 2),
            Path::new("/c/servers/rust-analyzer@-p-Catenary.2.jsonl")
        );
    }
}
