// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! JSONL firehose reader — the read half of `catenary query`.
//!
//! The observability rewrite (workstream 27) replaced the `messages` `SQLite`
//! firehose with append-only JSONL sharded by scope (see
//! [`crate::logging::jsonl_sink`]). This module is the reader side: it turns the
//! storage tree into query results.
//!
//! **The directory tree *is* the index.** Filters resolve to *file selection*,
//! not a content scan: `--session` / `--server` / `--tool` pick files by
//! `readdir` + name match, and `--since` prunes ts-prefixed call files before
//! they are ever opened. Only `--cwd` is a record-field filter (cwd is a record
//! field, not a directory layer — see the storage-tree amendment in ticket 00),
//! applied after open. Because it reads files directly with no socket, `query`
//! works **even when the daemon is down** and can never starve dispatch.
//!
//! **Output is raw.** Navigation is the *query* — you run a tighter or looser
//! filter, not expand nodes in a dump — so the firehose is presented flat: every
//! matching record, one line each, ascending by `ts`, nothing merged, collapsed,
//! or hidden. The TUI's pair-merge / scope-collapse were tree-navigation
//! concepts with no flat-output analog (and the record only carries `parent_id`
//! pairing, not the causation hierarchy the old display reconstructed);
//! request/response correlation stays in the data for a reader or `--search` to
//! follow.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

/// Compact, lexically-sortable timestamp format used for `grep`/`glob` call-file
/// name prefixes (`<ts>_<uuid>.jsonl`). Matches the `search_ts` the daemon mints
/// at the search-dispatch boundary (ticket 02 amendment). Because the fields are
/// fixed-width and zero-padded, string order equals chronological order — so the
/// `--since` prune is a lexical comparison, no parsing.
const COMPACT_TS_FMT: &str = "%Y%m%dT%H%M%S%3fZ";

/// One parsed firehose line.
///
/// The deserialize counterpart of the sink's `Record`. Unknown keys are ignored
/// (forward-compatible), and every optional key defaults to empty so a record
/// that omitted it (the sink skips empty keys) round-trips cleanly. Serializing
/// (`query --format json`) skips empty keys too, so output records match the
/// on-disk firehose shape.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Record {
    /// RFC3339 millis, UTC — the event time.
    pub ts: String,
    /// `lsp` | `mcp` | `hook` | `internal`.
    pub kind: String,
    /// `error` | `warn` | `info` | `debug`.
    pub level: String,
    /// Self-describing scope id (session id / search UUID / `server@root` /
    /// instance id).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope_id: String,
    /// Groups a request/response exchange (records of one exchange share it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// LSP server name, when relevant.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server: String,
    /// Workspace root for per-instance server identification.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope_root: String,
    /// Project shard + free `--cwd` filter dimension.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// Protocol method, or module target (internal events).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub method: String,
    /// Subsystem taxonomy (mainly internal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Nested protocol JSON (protocol events only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Rendered message (internal events only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Language id (internal events only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Remaining structured fields (internal events only).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub fields: Map<String, Value>,
}

/// Which stateless search command's per-invocation dir to select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `grep/` invocation files.
    Grep,
    /// `glob/` invocation files.
    Glob,
}

impl Tool {
    /// Parse a `--tool` value. Returns `None` for anything but `grep`/`glob`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "grep" => Some(Self::Grep),
            "glob" => Some(Self::Glob),
            _ => None,
        }
    }

    /// Subdirectory name under the instance root.
    const fn dir_name(self) -> &'static str {
        match self {
            Self::Grep => "grep",
            Self::Glob => "glob",
        }
    }
}

/// A fully-parsed query selection.
///
/// Bundles the file-selection axes plus the in-record filters. Duration
/// strings, level names, and tool names are parsed by the caller
/// ([`crate::cli::commands::run_query`]); this struct carries the typed results
/// so the reader never re-parses CLI surface.
#[derive(Debug, Default, Clone)]
pub struct Selection<'a> {
    /// Specific instance dir id; `None` = the freshest (most-recently-active)
    /// instance.
    pub instance: Option<&'a str>,
    /// Include every instance dir, not just the freshest one.
    pub all_instances: bool,
    /// Select a session's file (`sessions/<id>.jsonl`).
    pub session: Option<&'a str>,
    /// Select a server's file(s) (`servers/<server>[@root].jsonl`).
    pub server: Option<&'a str>,
    /// Select a search tool's invocation dir.
    pub tool: Option<Tool>,
    /// Drop records at or before this instant (and prune older call files).
    pub since: Option<DateTime<Utc>>,
    /// Minimum severity rank to keep (`error`=0 … `debug`=3); keep records whose
    /// rank is `<=` this.
    pub level: Option<u8>,
    /// Exact `kind` match (`lsp`/`mcp`/`hook`/`internal`).
    pub kind: Option<&'a str>,
    /// Case-insensitive substring over the record's searchable text.
    pub search: Option<&'a str>,
    /// Keep records whose `cwd` equals or is under this path.
    pub cwd: Option<&'a str>,
}

/// Severity rank for the `--level` threshold. Lower = more severe. Unknown
/// levels rank as the noisiest (`debug`), so an unrecognized tag is never
/// silently dropped by a threshold.
#[must_use]
pub fn level_rank(level: &str) -> u8 {
    match level {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        _ => 3,
    }
}

/// Parse a `--level` filter value into its threshold rank, rejecting unknown
/// names so a typo fails loud rather than silently matching nothing.
///
/// # Errors
///
/// Returns an error for any value other than `error`/`warn`/`info`/`debug`.
pub fn parse_level(s: &str) -> anyhow::Result<u8> {
    match s {
        "error" | "warn" | "info" | "debug" => Ok(level_rank(s)),
        other => Err(anyhow::anyhow!(
            "unknown --level {other} (expected error, warn, info, or debug)"
        )),
    }
}

/// The firehose tree root: `<cache_dir>/catenary`. Each child is one daemon
/// instance dir (`daemon:<uuid>`).
#[must_use]
pub fn firehose_root() -> PathBuf {
    crate::db::cache_dir().join("catenary")
}

/// Resolve the instance dirs a query reads.
///
/// - `instance = Some(id)` → exactly that dir (empty if absent).
/// - `all = true` → every instance dir, freshest-first.
/// - otherwise → the single **freshest** instance dir.
///
/// Freshness — not liveness. `query` cannot assert a daemon is alive (the only
/// heartbeat is MCP, and it is not concretely session-correlated; the honest
/// per-session recency signal is `last_seen` in `state.json`, not the firehose).
/// What it *can* assert is which instance was written most recently: each daemon
/// run mints a fresh `daemon:<uuid>` dir, and [`instance_freshness`] takes the
/// newest mtime among its entries (the top-level `mcp.jsonl` heartbeat keeps a
/// running daemon's dir fresh). That picks the current run's data whether the
/// daemon is up or down, without claiming it is up.
#[must_use]
pub fn resolve_instances(root: &Path, instance: Option<&str>, all: bool) -> Vec<PathBuf> {
    if let Some(id) = instance {
        let dir = root.join(id);
        return if dir.is_dir() { vec![dir] } else { Vec::new() };
    }

    let mut dirs: Vec<(SystemTime, PathBuf)> = read_dir_sorted(root)
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| (instance_freshness(&p), p))
        .collect();
    // Freshest first.
    dirs.sort_by_key(|(t, _)| std::cmp::Reverse(*t));

    if all {
        dirs.into_iter().map(|(_, p)| p).collect()
    } else {
        dirs.into_iter().take(1).map(|(_, p)| p).collect()
    }
}

/// Freshness of an instance dir: the newest mtime among the dir itself and its
/// immediate entries. Appends to the top-level `mcp.jsonl` heartbeat (and new
/// session/grep/glob shards) bump it, so a running daemon's dir stays fresh and
/// a dead one settles at its last write — an honest "most recently active",
/// not a liveness claim.
fn instance_freshness(dir: &Path) -> SystemTime {
    read_dir_sorted(dir)
        .iter()
        .map(|p| mtime(p))
        .chain(std::iter::once(mtime(dir)))
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Select the JSONL files an instance dir contributes for `sel`.
///
/// Scope axes are mutually exclusive and applied by precedence
/// (`session` > `server` > `tool`); absent all three, every file under the
/// instance is selected. `--since` prunes `grep`/`glob` call files by their
/// `<ts>` name prefix here, so unrelated time shards are never opened.
#[must_use]
pub fn select_files(dir: &Path, sel: &Selection<'_>) -> Vec<PathBuf> {
    if let Some(session) = sel.session {
        return session_files(dir, session);
    }
    if let Some(server) = sel.server {
        return server_files(dir, server);
    }
    if let Some(tool) = sel.tool {
        return tool_files(&dir.join(tool.dir_name()), sel.since);
    }
    all_files(dir, sel.since)
}

/// Files for `--session`: a direct open of `sessions/<id>.jsonl`, falling back
/// to a prefix match over the `sessions/` dir when no exact file exists (so a
/// short id prefix still resolves, as the old DB query allowed).
fn session_files(dir: &Path, session: &str) -> Vec<PathBuf> {
    let sessions = dir.join("sessions");
    let exact = sessions.join(format!("{session}.jsonl"));
    if exact.is_file() {
        return vec![exact];
    }
    read_dir_sorted(&sessions)
        .into_iter()
        .filter(|p| {
            file_name(p).is_some_and(|n| {
                is_jsonl(n)
                    && n.strip_suffix(".jsonl")
                        .is_some_and(|stem| stem.starts_with(session))
            })
        })
        .collect()
}

/// Files for `--server`: the rootless `servers/<server>.jsonl` plus every
/// rootful `servers/<server>@<enc-root>.jsonl`, **including rotation segments**.
///
/// Server files are Stream-class (ticket 01), so they roll into
/// `<stem>.<N>.jsonl` segments (`taplo.jsonl` → `taplo.2.jsonl`). [`base_stem`]
/// strips the `.jsonl` and the `.<N>` index, so the rootless base and every
/// segment share one verdict; a rootful file matches on the `<server>@` prefix.
fn server_files(dir: &Path, server: &str) -> Vec<PathBuf> {
    let servers = dir.join("servers");
    let rootful = format!("{server}@");
    read_dir_sorted(&servers)
        .into_iter()
        .filter(|p| {
            file_name(p)
                .and_then(base_stem)
                .is_some_and(|stem| stem == server || stem.starts_with(&rootful))
        })
        .collect()
}

/// The base scope stem of a firehose file name: strip the `.jsonl` extension and
/// any trailing `.<N>` rotation segment index. Returns `None` for a non-`.jsonl`
/// name.
///
/// Stream-class files roll into `<stem>.<N>.jsonl` segments (ticket 01), and
/// server names / encoded roots contain no `.` (the cwd encoding maps `.` → `-`),
/// so the only dotted suffix on the bare name is a rotation index — the same
/// contract the reaper's `strip_segment_index` relies on.
///
/// `taplo.2.jsonl` → `taplo`; `rust-analyzer@-home-p.jsonl` → `rust-analyzer@-home-p`.
fn base_stem(name: &str) -> Option<&str> {
    let bare = name.strip_suffix(".jsonl")?;
    Some(match bare.rsplit_once('.') {
        Some((stem, idx)) if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) => stem,
        _ => bare,
    })
}

/// Files for `--tool`: every `.jsonl` in the tool dir, with `--since` pruning by
/// the `<ts>` name prefix (lexical, since the format is fixed-width).
fn tool_files(tool_dir: &Path, since: Option<DateTime<Utc>>) -> Vec<PathBuf> {
    let cutoff = since.map(|c| c.format(COMPACT_TS_FMT).to_string());
    read_dir_sorted(tool_dir)
        .into_iter()
        .filter(|p| file_name(p).is_some_and(is_jsonl))
        .filter(|p| !pruned_by_since(p, cutoff.as_deref()))
        .collect()
}

/// Every JSONL file an instance contributes when no scope axis is set: the
/// instance-global streams, all server files, all session files, and the
/// (since-pruned) `grep`/`glob` call files.
fn all_files(dir: &Path, since: Option<DateTime<Utc>>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = read_dir_sorted(dir)
        .into_iter()
        .filter(|p| p.is_file() && file_name(p).is_some_and(is_jsonl))
        .collect();
    out.extend(jsonl_in(&dir.join("servers")));
    out.extend(jsonl_in(&dir.join("sessions")));
    out.extend(tool_files(&dir.join("grep"), since));
    out.extend(tool_files(&dir.join("glob"), since));
    out
}

/// Non-recursive list of `.jsonl` files in `dir` (empty if absent).
fn jsonl_in(dir: &Path) -> Vec<PathBuf> {
    read_dir_sorted(dir)
        .into_iter()
        .filter(|p| file_name(p).is_some_and(is_jsonl))
        .collect()
}

/// Whether a `<ts>_<uuid>.jsonl` call file is older than the `--since` cutoff and
/// can be skipped without opening. A file with no `<ts>` prefix (bare
/// `<uuid>.jsonl`) or an absent cutoff is never pruned.
fn pruned_by_since(path: &Path, cutoff: Option<&str>) -> bool {
    let Some(cutoff) = cutoff else { return false };
    let Some(name) = file_name(path) else {
        return false;
    };
    // `<ts>_<uuid>.jsonl` — the part before `_` is the ts prefix. Neither the
    // compact ts nor the simple-form uuid contains `_`, so this split is clean.
    name.split_once('_').is_some_and(|(ts, _)| ts < cutoff)
}

/// Parse the complete (newline-terminated) lines of `content` into records.
///
/// A torn final line (no trailing `\n`, e.g. a daemon crash mid-append) is
/// skipped, not an error. A complete-but-unparseable line is skipped too
/// (defensive — protocol lines are always valid JSON).
#[must_use]
pub fn parse_complete_lines(content: &str) -> Vec<Record> {
    let mut out = Vec::new();
    for seg in content.split_inclusive('\n') {
        if !seg.ends_with('\n') {
            // Unterminated tail — torn write. Stop here.
            break;
        }
        let line = seg.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<Record>(line) {
            out.push(rec);
        }
    }
    out
}

/// Read and filter records from the selected files, sorted ascending by `ts`.
///
/// Each file is read whole, parsed line-by-line (torn/invalid lines skipped),
/// and every record is run through the in-record filters in `sel`
/// (`--since`/`--level`/`--kind`/`--cwd`/`--search`). A file that cannot be read
/// is skipped silently — the firehose is regenerable, and a vanished shard is
/// not an error for a forensic reader.
#[must_use]
pub fn read_records(files: &[PathBuf], sel: &Selection<'_>) -> Vec<Record> {
    let mut out = Vec::new();
    for file in files {
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        for rec in parse_complete_lines(&content) {
            if record_matches(&rec, sel) {
                out.push(rec);
            }
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    out
}

/// Apply the in-record (post-open) filters to one record.
fn record_matches(rec: &Record, sel: &Selection<'_>) -> bool {
    if let Some(cutoff) = sel.since
        && let Ok(ts) = DateTime::parse_from_rfc3339(&rec.ts)
        && ts.with_timezone(&Utc) <= cutoff
    {
        return false;
    }
    if let Some(threshold) = sel.level
        && level_rank(&rec.level) > threshold
    {
        return false;
    }
    if let Some(kind) = sel.kind
        && rec.kind != kind
    {
        return false;
    }
    if let Some(cwd) = sel.cwd
        && !(rec.cwd == cwd || rec.cwd.starts_with(&format!("{cwd}/")))
    {
        return false;
    }
    if let Some(needle) = sel.search
        && !haystack(rec)
            .to_lowercase()
            .contains(&needle.to_lowercase())
    {
        return false;
    }
    true
}

/// The searchable text of a record for `--search`: method, server, scope id,
/// rendered message, and the serialized payload/fields.
fn haystack(rec: &Record) -> String {
    let mut s = format!("{} {} {}", rec.method, rec.server, rec.scope_id);
    if let Some(msg) = &rec.message {
        s.push(' ');
        s.push_str(msg);
    }
    if let Some(payload) = &rec.payload {
        s.push(' ');
        s.push_str(&payload.to_string());
    }
    if !rec.fields.is_empty() {
        s.push(' ');
        s.push_str(&Value::Object(rec.fields.clone()).to_string());
    }
    s
}

/// One-shot query: resolve instances, select + read + filter files, and return
/// every matching record in raw chronological order.
///
/// **Navigation is the query, not the output.** You narrow or widen the filters
/// (`--session`/`--server`/`--tool`/`--since`/`--search`) to run a tighter or
/// looser query — there is no in-output structure to expand. So the firehose is
/// presented flat: every matching line, ascending by `ts`, nothing merged,
/// collapsed, or hidden. (Request/response correlation is still in the data —
/// records of one exchange share a `parent_id` — for a reader or `--search` to
/// follow; `query` just never folds it.)
#[must_use]
pub fn gather(root: &Path, sel: &Selection<'_>) -> Vec<Record> {
    let mut records: Vec<Record> = Vec::new();
    for instance in resolve_instances(root, sel.instance, sel.all_instances) {
        let files = select_files(&instance, sel);
        records.extend(read_records(&files, sel));
    }
    records.sort_by(|a, b| a.ts.cmp(&b.ts));
    records
}

/// Live tail (`--follow`) over the current file selection.
///
/// Tracks a byte offset per file. [`Follower::new`] seeds offsets to each
/// selected file's current end, so the first [`Follower::poll`] returns only
/// records appended afterward. Each poll re-resolves the selection, so a brand
/// new shard (a fresh session or `grep` invocation) is picked up and read from
/// its start. A torn trailing line is left unconsumed and re-read once complete.
pub struct Follower<'a> {
    root: PathBuf,
    sel: Selection<'a>,
    offsets: HashMap<PathBuf, u64>,
}

impl<'a> Follower<'a> {
    /// Create a follower seeded at the current end of every selected file, so
    /// only newly-appended records surface.
    #[must_use]
    pub fn new(root: &Path, sel: Selection<'a>) -> Self {
        let mut offsets = HashMap::new();
        for instance in resolve_instances(root, sel.instance, sel.all_instances) {
            for file in select_files(&instance, &sel) {
                let len = file.metadata().map_or(0, |m| m.len());
                offsets.insert(file, len);
            }
        }
        Self {
            root: root.to_path_buf(),
            sel,
            offsets,
        }
    }

    /// Read every complete record appended since the last poll, ascending by
    /// `ts`. New files (not yet tracked) are read from their start.
    pub fn poll(&mut self) -> Vec<Record> {
        let mut new = Vec::new();
        for instance in resolve_instances(&self.root, self.sel.instance, self.sel.all_instances) {
            for file in select_files(&instance, &self.sel) {
                self.read_appended(&file, &mut new);
            }
        }
        new.sort_by(|a, b| a.ts.cmp(&b.ts));
        new
    }

    /// Read complete lines appended to one file past its tracked offset,
    /// advancing the offset past the last newline (so a torn tail is retried).
    fn read_appended(&mut self, file: &Path, out: &mut Vec<Record>) {
        let mut start = self.offsets.get(file).copied().unwrap_or(0);
        let Ok(mut handle) = File::open(file) else {
            return;
        };
        let len = handle.metadata().map_or(0, |m| m.len());
        // File shrank (rotation/truncation): restart from the beginning.
        if len < start {
            start = 0;
        }
        if handle.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if handle.read_to_end(&mut bytes).is_err() {
            return;
        }
        // Consume only through the last complete line.
        let consumed = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
        if consumed > 0 {
            let text = String::from_utf8_lossy(&bytes[..consumed]);
            for rec in parse_complete_lines(&text) {
                if record_matches(&rec, &self.sel) {
                    out.push(rec);
                }
            }
        }
        self.offsets
            .insert(file.to_path_buf(), start + consumed as u64);
    }
}

// ── small fs helpers ─────────────────────────────────────────────────────

/// Sorted directory listing (by path) — deterministic output and tests. An
/// unreadable/absent dir yields an empty list.
fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    paths
}

/// File name as `&str`, or `None` for a non-UTF-8 / empty name.
fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|n| n.to_str())
}

/// Whether a file name denotes a JSONL firehose file. Base files and rotation
/// segments alike end in `.jsonl` (segments are `<stem>.<N>.jsonl`, ticket 01),
/// so a suffix check covers both.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "firehose files are always written with a lowercase .jsonl extension"
)]
fn is_jsonl(name: &str) -> bool {
    name.ends_with(".jsonl")
}

/// Directory/file mtime, or [`SystemTime::UNIX_EPOCH`] when unavailable (sorts
/// such entries oldest).
fn mtime(path: &Path) -> SystemTime {
    path.metadata()
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use std::fs;

    use chrono::TimeZone as _;

    use super::*;

    /// Build a minimal record line for a session file.
    fn line(ts: &str, kind: &str, extra: &str) -> String {
        format!(r#"{{"ts":"{ts}","kind":"{kind}","level":"info"{extra}}}"#)
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, contents).expect("write");
    }

    fn instance(root: &Path) -> PathBuf {
        root.join("daemon:test")
    }

    // ── parsing ──────────────────────────────────────────────────────

    #[test]
    fn torn_final_line_is_skipped() {
        let content = format!(
            "{}\n{}",
            line("2026-06-09T10:00:00.000Z", "internal", r#","message":"ok""#),
            // No trailing newline → torn.
            line(
                "2026-06-09T10:00:01.000Z",
                "internal",
                r#","message":"torn""#
            ),
        );
        let recs = parse_complete_lines(&content);
        assert_eq!(recs.len(), 1, "torn tail dropped: {recs:?}");
        assert_eq!(recs[0].message.as_deref(), Some("ok"));
    }

    #[test]
    fn complete_but_invalid_line_is_skipped() {
        let content = format!(
            "not json\n{}\n",
            line("2026-06-09T10:00:00.000Z", "internal", r#","message":"ok""#),
        );
        let recs = parse_complete_lines(&content);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].message.as_deref(), Some("ok"));
    }

    // ── file selection (the tree is the index) ───────────────────────

    #[test]
    fn session_filter_opens_only_that_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        write(
            &inst.join("sessions").join("s1.jsonl"),
            &format!("{}\n", line("2026-06-09T10:00:00.000Z", "hook", "")),
        );
        write(
            &inst.join("sessions").join("s2.jsonl"),
            &format!("{}\n", line("2026-06-09T10:00:00.000Z", "hook", "")),
        );
        write(
            &inst.join("servers").join("taplo.jsonl"),
            &format!("{}\n", line("2026-06-09T10:00:00.000Z", "lsp", "")),
        );

        let sel = Selection {
            session: Some("s1"),
            ..Selection::default()
        };
        let files = select_files(&inst, &sel);
        assert_eq!(files, vec![inst.join("sessions").join("s1.jsonl")]);
    }

    #[test]
    fn server_filter_selects_rootless_rootful_and_segments() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        let servers = inst.join("servers");
        for name in [
            "rust-analyzer.jsonl",           // rootless base
            "rust-analyzer.2.jsonl",         // rootless rotation segment
            "rust-analyzer@-home-p.jsonl",   // rootful base
            "rust-analyzer@-home-p.1.jsonl", // rootful rotation segment
            "taplo.jsonl",                   // unrelated server
            "rust-analyzer-extra.jsonl",     // name prefix, but a different server
        ] {
            write(
                &servers.join(name),
                &format!("{}\n", line("2026-06-09T10:00:00.000Z", "lsp", "")),
            );
        }
        let sel = Selection {
            server: Some("rust-analyzer"),
            ..Selection::default()
        };
        let mut files = select_files(&inst, &sel);
        files.sort();
        assert_eq!(
            files,
            vec![
                servers.join("rust-analyzer.2.jsonl"),
                servers.join("rust-analyzer.jsonl"),
                servers.join("rust-analyzer@-home-p.1.jsonl"),
                servers.join("rust-analyzer@-home-p.jsonl"),
            ],
            "rootless + rootful bases and their .N segments, but not taplo or \
             rust-analyzer-extra",
        );
    }

    #[test]
    fn tool_filter_selects_only_that_dir() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        write(
            &inst.join("grep").join("20260609T100000000Z_abc.jsonl"),
            &format!("{}\n", line("2026-06-09T10:00:00.000Z", "hook", "")),
        );
        write(
            &inst.join("glob").join("20260609T100000000Z_def.jsonl"),
            &format!("{}\n", line("2026-06-09T10:00:00.000Z", "hook", "")),
        );
        let sel = Selection {
            tool: Some(Tool::Grep),
            ..Selection::default()
        };
        let files = select_files(&inst, &sel);
        assert_eq!(
            files,
            vec![inst.join("grep").join("20260609T100000000Z_abc.jsonl")]
        );
    }

    #[test]
    fn since_prunes_old_call_files_without_selecting_them() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        let old = inst.join("grep").join("20260101T000000000Z_old.jsonl");
        let new = inst.join("grep").join("20260609T120000000Z_new.jsonl");
        write(
            &old,
            &format!("{}\n", line("2026-01-01T00:00:00.000Z", "hook", "")),
        );
        write(
            &new,
            &format!("{}\n", line("2026-06-09T12:00:00.000Z", "hook", "")),
        );

        let cutoff = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("unambiguous cutoff");
        let sel = Selection {
            tool: Some(Tool::Grep),
            since: Some(cutoff),
            ..Selection::default()
        };
        let files = select_files(&inst, &sel);
        assert_eq!(files, vec![new], "old shard pruned, not opened");
    }

    #[test]
    fn default_selection_covers_the_whole_instance() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        let one = format!("{}\n", line("2026-06-09T10:00:00.000Z", "mcp", ""));
        write(&inst.join("mcp.jsonl"), &one);
        write(&inst.join("trace.jsonl"), &one);
        write(&inst.join("servers").join("taplo.jsonl"), &one);
        write(&inst.join("sessions").join("s1.jsonl"), &one);
        write(&inst.join("grep").join("20260609T100000000Z_a.jsonl"), &one);

        let files = select_files(&inst, &Selection::default());
        assert_eq!(files.len(), 5, "all shards selected: {files:?}");
    }

    // ── instance resolution ──────────────────────────────────────────

    #[test]
    fn default_instance_is_the_freshest_dir() {
        let dir = tempfile::tempdir().expect("tmp");
        let stale = dir.path().join("daemon:stale");
        let fresh = dir.path().join("daemon:fresh");
        write(&stale.join("mcp.jsonl"), "x");
        // A short gap so `fresh`'s heartbeat mtime is strictly later than
        // `stale`'s (tmpfs/ext4 give sub-ms mtime resolution).
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&fresh.join("mcp.jsonl"), "x");

        let default = resolve_instances(dir.path(), None, false);
        assert_eq!(default, vec![fresh.clone()], "freshest by activity");

        let all = resolve_instances(dir.path(), None, true);
        assert_eq!(all.len(), 2, "all-instances includes both");
        assert_eq!(all[0], fresh, "freshest first");
    }

    // ── in-record filters ────────────────────────────────────────────

    #[test]
    fn cwd_filter_keeps_matching_and_subpaths() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        let f = inst.join("sessions").join("s1.jsonl");
        let body = [
            line("2026-06-09T10:00:00.000Z", "hook", r#","cwd":"/proj""#),
            line("2026-06-09T10:00:01.000Z", "hook", r#","cwd":"/proj/src""#),
            line("2026-06-09T10:00:02.000Z", "hook", r#","cwd":"/other""#),
        ]
        .join("\n");
        write(&f, &format!("{body}\n"));

        let sel = Selection {
            session: Some("s1"),
            cwd: Some("/proj"),
            ..Selection::default()
        };
        let recs = read_records(&select_files(&inst, &sel), &sel);
        assert_eq!(recs.len(), 2, "kept /proj and /proj/src, dropped /other");
    }

    #[test]
    fn level_filter_is_a_severity_threshold() {
        let recs = [
            rec_level("error"),
            rec_level("warn"),
            rec_level("info"),
            rec_level("debug"),
        ];
        let sel = Selection {
            level: Some(level_rank("warn")),
            ..Selection::default()
        };
        let kept: Vec<&str> = recs
            .iter()
            .filter(|r| record_matches(r, &sel))
            .map(|r| r.level.as_str())
            .collect();
        assert_eq!(kept, vec!["error", "warn"]);
    }

    fn rec_level(level: &str) -> Record {
        Record {
            ts: "2026-06-09T10:00:00.000Z".into(),
            kind: "internal".into(),
            level: level.into(),
            scope_id: String::new(),
            parent_id: None,
            server: String::new(),
            scope_root: String::new(),
            cwd: String::new(),
            method: String::new(),
            source: None,
            payload: None,
            message: Some("m".into()),
            language: None,
            fields: Map::new(),
        }
    }

    #[test]
    fn search_is_case_insensitive_over_payload() {
        let mut r = rec_level("info");
        r.kind = "lsp".into();
        r.payload = Some(serde_json::json!({"method": "textDocument/Hover"}));
        let hit = Selection {
            search: Some("hover"),
            ..Selection::default()
        };
        let miss = Selection {
            search: Some("definition"),
            ..Selection::default()
        };
        assert!(record_matches(&r, &hit));
        assert!(!record_matches(&r, &miss));
    }

    // ── gather + follow ──────────────────────────────────────────────

    #[test]
    fn gather_reads_raw_chronological_without_a_daemon() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        // Out-of-order on disk to prove gather sorts by ts.
        let body = [
            line("2026-06-09T10:00:01.000Z", "hook", r#","message":"b""#),
            line("2026-06-09T10:00:00.000Z", "hook", r#","message":"a""#),
        ]
        .join("\n");
        write(
            &inst.join("sessions").join("s1.jsonl"),
            &format!("{body}\n"),
        );
        // No daemon, no socket — pure file read.
        let records = gather(
            dir.path(),
            &Selection {
                session: Some("s1"),
                ..Selection::default()
            },
        );
        // Every record, no merge/collapse, ascending by ts.
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].message.as_deref(), Some("a"));
        assert_eq!(records[1].message.as_deref(), Some("b"));
    }

    #[test]
    fn follow_emits_only_newly_appended_records() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        let file = inst.join("sessions").join("s1.jsonl");
        write(
            &file,
            &format!(
                "{}\n",
                line("2026-06-09T10:00:00.000Z", "hook", r#","message":"old""#)
            ),
        );

        let sel = Selection {
            session: Some("s1"),
            ..Selection::default()
        };
        let mut follower = Follower::new(dir.path(), sel);
        // Seeded at EOF: nothing new yet.
        assert!(follower.poll().is_empty(), "no records before an append");

        // Append a line.
        let mut handle = fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .expect("open");
        writeln!(
            handle,
            "{}",
            line("2026-06-09T10:00:05.000Z", "hook", r#","message":"new""#)
        )
        .expect("append");

        let got = follower.poll();
        assert_eq!(got.len(), 1, "the appended record surfaces");
        assert_eq!(got[0].message.as_deref(), Some("new"));
        // A subsequent poll with no append is empty.
        assert!(follower.poll().is_empty());
    }

    #[test]
    fn follow_picks_up_a_brand_new_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let inst = instance(dir.path());
        write(&inst.join("sessions").join("s1.jsonl"), "");
        let mut follower = Follower::new(
            dir.path(),
            Selection::default(), // whole instance
        );
        assert!(follower.poll().is_empty());

        // A new session file appears after the follower started.
        write(
            &inst.join("sessions").join("s2.jsonl"),
            &format!(
                "{}\n",
                line("2026-06-09T10:00:00.000Z", "hook", r#","message":"fresh""#)
            ),
        );
        let got = follower.poll();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message.as_deref(), Some("fresh"));
    }
}
