// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CLI subcommands: list, monitor, status, query, gc, and ls-roots.

#![allow(
    clippy::print_stderr,
    reason = "find_session prints ambiguous matches to stderr"
)]

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use regex::Regex;
use std::time::Duration;

use crate::cli::{self, ColorConfig, ColumnWidths, Output, QueryFormat, jsonl_reader};
use crate::session::{self, SessionMessage};

/// List all active sessions.
///
/// # Errors
///
/// Returns an error if listing sessions fails.
pub fn run_list(out: &mut Output) -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    let sessions = session::list_sessions_with_conn(&conn)?;

    if sessions.is_empty() {
        let _ = out.writeln(format_args!("No active Catenary sessions"));
        return Ok(());
    }

    let widths = ColumnWidths::calculate(out.width);

    // Print header
    let _ = out.writeln(format_args!(
        "{:>width_num$} {:<width_id$} {:<width_pid$} {:<width_client$} {:<width_ws$} STARTED",
        "#",
        "ID",
        "PID",
        "CLIENT",
        "WORKSPACE",
        width_num = widths.row_num,
        width_id = widths.id,
        width_pid = widths.pid,
        width_client = widths.client,
        width_ws = widths.workspace,
    ));
    let _ = out.writeln(format_args!("{}", "-".repeat(out.width.min(120))));

    // Indent for the second line (aligns with ID column)
    let indent = " ".repeat(widths.row_num + 1);

    for (idx, (s, alive)) in sessions.iter().enumerate() {
        let client = match (&s.client_name, &s.client_version) {
            (Some(name), Some(ver)) => format!("{name} v{ver}"),
            (Some(name), None) => name.clone(),
            _ => "-".to_string(),
        };

        let ago = format_duration_ago(s.started_at);

        // Truncate fields to fit column widths
        let id = cli::truncate(&s.id, widths.id);
        let workspace = cli::truncate(&s.workspace, widths.workspace);
        let client = cli::truncate(&client, widths.client);

        let row_str = format!(
            "{:>width_num$} {:<width_id$} {:<width_pid$} {:<width_client$} {:<width_ws$} {}",
            idx + 1,
            id,
            s.pid,
            client,
            workspace,
            ago,
            width_num = widths.row_num,
            width_id = widths.id,
            width_pid = widths.pid,
            width_client = widths.client,
            width_ws = widths.workspace,
        );

        if *alive {
            let _ = out.writeln(format_args!("{row_str}"));
            // Get active languages for this session (shown on second line)
            let languages = session::active_languages_with_conn(&conn, &s.id).unwrap_or_default();
            if !languages.is_empty() {
                let lang_str = languages.join(", ");
                let _ = out.writeln(format_args!(
                    "{}",
                    out.colors
                        .dim(&format!("{indent}language servers: {lang_str}"))
                ));
            }
        } else {
            let _ = out.writeln(format_args!("{} (dead)", out.colors.dim(&row_str)));
        }
    }

    Ok(())
}

/// Resolve a session ID from either a row number or ID prefix.
///
/// # Errors
///
/// Returns an error if the session cannot be found.
pub fn resolve_session_id(conn: &rusqlite::Connection, id: &str) -> Result<session::SessionInfo> {
    // Try parsing as a row number first (1-indexed)
    if let Ok(row_num) = id.parse::<usize>()
        && row_num > 0
    {
        let sessions = session::list_sessions_with_conn(conn)?;
        if let Some((s, _)) = sessions.get(row_num - 1) {
            return Ok(s.clone());
        }
        // Row number out of range — try as session ID prefix before giving up.
        // Session IDs are hex strings that may be all digits (e.g., "025586387"),
        // so a purely numeric input could be either a row number or a session ID.
        if let Ok(session) = find_session(conn, id) {
            return Ok(session);
        }
        anyhow::bail!("Row number {} out of range (1-{})", row_num, sessions.len());
    }

    // Fall back to find_session (ID prefix matching)
    find_session(conn, id)
}

/// Monitor events from a session.
///
/// # Errors
///
/// Returns an error if the session cannot be found or monitoring fails.
pub fn run_monitor(out: &mut Output, id: &str, raw: bool, filter: Option<&str>) -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    // Resolve session ID (supports row numbers and prefix matching)
    let session = resolve_session_id(&conn, id)?;
    let full_id = session.id;

    // Compile filter regex if provided
    let filter_regex = filter
        .as_ref()
        .map(|f| Regex::new(f))
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid filter regex: {e}"))?;

    let _ = out.writeln(format_args!(
        "Monitoring session {full_id} (Ctrl+C to stop)\n"
    ));

    let mut reader = session::tail_messages_new(&full_id, true)?;

    // Track last progress key (server, title) for line collapsing.
    let mut last_progress: Option<(String, String)> = None;

    loop {
        match reader.try_next_message() {
            Ok(Some(msg)) => {
                // Apply filter if set
                if let Some(ref re) = filter_regex {
                    let msg_str = format!("{} {} {}", msg.r#type, msg.method, msg.server);
                    if !re.is_match(&msg_str) {
                        continue;
                    }
                }

                if raw {
                    print_message_raw(out, &msg);
                } else {
                    // Collapse consecutive progress lines with the same title
                    if msg.r#type == "lsp" && msg.method == "$/progress" {
                        let title = msg
                            .payload
                            .get("value")
                            .and_then(|v| v.get("title"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let key = (msg.server.clone(), title);
                        if last_progress.as_ref() == Some(&key) {
                            let _ = out.write_str(format_args!("\x1b[A\x1b[2K"));
                        }
                        last_progress = Some(key);
                    } else {
                        last_progress = None;
                    }
                    print_message_annotated(out, &msg);
                }
            }
            Ok(None) => {
                // No new message — check liveness
                std::thread::sleep(Duration::from_millis(100));

                if let Ok(Some((_, alive))) = session::get_session_with_conn(&conn, &full_id) {
                    if !alive {
                        let _ = out.writeln(format_args!("\nSession ended (process dead)"));
                        break;
                    }
                } else {
                    let _ = out.writeln(format_args!("\nSession ended (files removed)"));
                    break;
                }
            }
            Err(_) => {
                let _ = out.writeln(format_args!("\nSession ended"));
                break;
            }
        }
    }

    Ok(())
}

/// Show status of a session.
///
/// # Errors
///
/// Returns an error if the session cannot be found.
pub fn run_status(out: &mut Output, id: &str) -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    let session = find_session(&conn, id)?;

    let _ = out.writeln(format_args!("Session: {}", session.id));
    let _ = out.writeln(format_args!("PID: {}", session.pid));
    let _ = out.writeln(format_args!("Workspace: {}", session.workspace));
    let _ = out.writeln(format_args!(
        "Started: {} ({})",
        session
            .started_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S"),
        format_duration_ago(session.started_at)
    ));

    if let Some(name) = &session.client_name {
        if let Some(ver) = &session.client_version {
            let _ = out.writeln(format_args!("Client: {name} v{ver}"));
        } else {
            let _ = out.writeln(format_args!("Client: {name}"));
        }
    }

    // Show recent messages
    let _ = out.writeln(format_args!("\nRecent messages:"));
    let messages = session::monitor_messages_with_conn(&conn, &session.id, true)?;
    let recent: Vec<_> = messages.iter().rev().take(10).collect();

    for msg in recent.iter().rev() {
        print_message_annotated(out, msg);
    }

    Ok(())
}

/// List all tracked workspace roots with their source.
///
/// Connects to the daemon's IPC socket and sends a `tool/roots-ls`
/// request, then prints each root with its contributor sources.
///
/// # Errors
///
/// Returns an error if no daemon is running or the response is invalid.
pub async fn run_ls_roots(out: &mut Output) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = crate::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/roots-ls"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let response: serde_json::Value =
        serde_json::from_str(line.trim()).context("invalid response from daemon")?;

    let roots = response
        .get("roots")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if roots.is_empty() {
        let _ = out.writeln(format_args!("No tracked roots"));
        return Ok(());
    }

    for entry in &roots {
        let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let sources: Vec<&str> = entry
            .get("sources")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|s| s.as_str()).collect::<Vec<&str>>())
            .unwrap_or_default();

        let source_label = if sources.is_empty() {
            "unknown".to_string()
        } else {
            sources.join(", ")
        };

        let _ = out.writeln(format_args!(
            "{path}  {}",
            out.colors.dim(&format!("[{source_label}]"))
        ));
    }

    Ok(())
}

/// Parse a human-friendly duration string into a UTC cutoff timestamp.
///
/// Accepted formats:
/// - `Nm` — N minutes ago
/// - `Nh` — N hours ago
/// - `Nd` — N days ago
/// - `today` — midnight local time today
///
/// # Errors
///
/// Returns an error if the string is not in a recognised format.
pub(crate) fn parse_since(s: &str) -> Result<chrono::DateTime<Utc>> {
    if s == "today" {
        let today = Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("failed to compute midnight"))?;
        let local_midnight = today
            .and_local_timezone(Local)
            .single()
            .ok_or_else(|| anyhow::anyhow!("ambiguous local midnight"))?;
        return Ok(local_midnight.with_timezone(&Utc));
    }

    let (digits, unit) = s
        .strip_suffix('m')
        .map(|d| (d, "m"))
        .or_else(|| s.strip_suffix('h').map(|d| (d, "h")))
        .or_else(|| s.strip_suffix('d').map(|d| (d, "d")))
        .ok_or_else(|| {
            anyhow::anyhow!("unrecognised duration: {s} (expected Nm, Nh, Nd, or today)")
        })?;

    let n: i64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid number in duration: {s}"))?;

    let duration = match unit {
        "m" => chrono::Duration::minutes(n),
        "h" => chrono::Duration::hours(n),
        "d" => chrono::Duration::days(n),
        _ => unreachable!(),
    };

    Ok(Utc::now() - duration)
}

/// Parsed `catenary query` arguments, threaded from the top-level CLI surface
/// in `main.rs` into the JSONL firehose reader.
///
/// File-selection axes (`session`/`server`/`tool`/`instance`) and in-record
/// filters (`cwd`/`since`/`level`/`kind`/`search`) are forwarded verbatim;
/// duration/level/tool parsing happens in [`run_query`].
pub struct QueryArgs<'a> {
    /// Session id (or prefix) → `sessions/<id>.jsonl`.
    pub session: Option<&'a str>,
    /// Server name → `servers/<server>[@root].jsonl`.
    pub server: Option<&'a str>,
    /// `grep` or `glob` → that tool's invocation dir.
    pub tool: Option<&'a str>,
    /// Record-field filter: keep records whose `cwd` equals or is under this.
    pub cwd: Option<&'a str>,
    /// Time filter (`1h`, `today`, `7d`, `30m`).
    pub since: Option<&'a str>,
    /// Specific instance dir id; default is the freshest instance.
    pub instance: Option<&'a str>,
    /// Include every instance dir, not just the freshest one.
    pub all_instances: bool,
    /// Minimum severity (`error`/`warn`/`info`/`debug`).
    pub level: Option<&'a str>,
    /// Exact record kind (`lsp`/`mcp`/`hook`/`internal`).
    pub kind: Option<&'a str>,
    /// Case-insensitive free-text substring.
    pub search: Option<&'a str>,
    /// Live-tail mode.
    pub follow: bool,
    /// Max rows rendered (0 = unlimited).
    pub limit: usize,
    /// Output format.
    pub format: QueryFormat,
}

/// Poll cadence for `--follow`.
const FOLLOW_POLL: Duration = Duration::from_millis(200);

/// Query the JSONL firehose.
///
/// Reads the sharded append-only logs directly (no daemon, no socket — works
/// even when the daemon is down), selecting files by the scope axes and
/// filtering records by the in-record dimensions. Output is raw: one-shot mode
/// renders the most recent `limit` records in chronological order, nothing
/// merged or collapsed; `--follow` tails the selection live.
///
/// # Errors
///
/// Returns an error only for malformed filter arguments (`--since`, `--level`,
/// `--tool`). A missing/unreadable firehose is not an error — it yields no
/// results.
pub fn run_query(out: &mut Output, args: &QueryArgs<'_>) -> Result<()> {
    let since = args.since.map(parse_since).transpose()?;
    let tool = args
        .tool
        .map(|t| {
            jsonl_reader::Tool::parse(t)
                .ok_or_else(|| anyhow::anyhow!("unknown --tool {t} (expected grep or glob)"))
        })
        .transpose()?;
    let level = args.level.map(jsonl_reader::parse_level).transpose()?;

    let sel = jsonl_reader::Selection {
        instance: args.instance,
        all_instances: args.all_instances,
        session: args.session,
        server: args.server,
        tool,
        since,
        level,
        kind: args.kind,
        search: args.search,
        cwd: args.cwd,
    };

    let root = jsonl_reader::firehose_root();

    if args.follow {
        return run_follow(out, &root, sel);
    }

    let mut records = jsonl_reader::gather(&root, &sel);
    // Keep the most recent `limit` records (0 = unlimited) — a tail — but render
    // them in chronological order, consistent with `--follow`.
    if args.limit != 0 && records.len() > args.limit {
        records = records.split_off(records.len() - args.limit);
    }
    render_rows(out, &records, args.format);
    Ok(())
}

/// Live-tail loop: poll the selection and print each newly-appended record as a
/// single line until interrupted. Returns only on a write error; Ctrl-C ends it.
fn run_follow(
    out: &mut Output,
    root: &std::path::Path,
    sel: jsonl_reader::Selection<'_>,
) -> Result<()> {
    use std::io::Write as _;
    let mut follower = jsonl_reader::Follower::new(root, sel);
    loop {
        for rec in follower.poll() {
            out.writeln(format_args!("{}", follow_line(&rec)))?;
        }
        out.flush().ok();
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Table column headers, in cell order.
const QUERY_HEADERS: [&str; 7] = [
    "TIME", "LEVEL", "KIND", "SCOPE", "SERVER", "METHOD", "SUMMARY",
];

/// Render the gathered records in the chosen format.
fn render_rows(out: &mut Output, records: &[jsonl_reader::Record], format: QueryFormat) {
    match format {
        QueryFormat::Table => render_table(out, records),
        QueryFormat::Json => render_json(out, records),
    }
}

/// Render records as an aligned table (the default surface) — one line each,
/// raw and chronological.
fn render_table(out: &mut Output, records: &[jsonl_reader::Record]) {
    if records.is_empty() {
        let _ = out.writeln(format_args!("No results"));
        return;
    }

    let cells: Vec<Vec<String>> = records.iter().map(record_cells).collect();
    let mut widths: Vec<usize> = QUERY_HEADERS.iter().map(|h| h.chars().count()).collect();
    for row in &cells {
        for (i, val) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(val.chars().count());
            }
        }
    }
    // Cap the free-form SUMMARY column.
    if let Some(last) = widths.last_mut() {
        *last = (*last).min(80);
    }

    let header: Vec<String> = QUERY_HEADERS
        .iter()
        .zip(&widths)
        .map(|(name, w)| format!("{name:<w$}"))
        .collect();
    let _ = out.writeln(format_args!("{}", header.join("  ")));
    let _ = out.writeln(format_args!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    ));

    for row in &cells {
        let formatted: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(val, w)| {
                let clipped = clip(val, *w);
                format!("{clipped:<w$}")
            })
            .collect();
        let _ = out.writeln(format_args!("{}", formatted.join("  ")));
    }
}

/// Render records as a JSON array — the raw firehose lines (empty keys omitted,
/// matching the on-disk shape), in chronological order.
fn render_json(out: &mut Output, records: &[jsonl_reader::Record]) {
    let json = serde_json::to_string_pretty(records).unwrap_or_default();
    let _ = out.writeln(format_args!("{json}"));
}

/// The seven table cells for one record.
fn record_cells(rec: &jsonl_reader::Record) -> Vec<String> {
    vec![
        local_hms(&rec.ts),
        rec.level.clone(),
        rec.kind.clone(),
        clip(&rec.scope_id, 14),
        rec.server.clone(),
        rec.method.clone(),
        summary(rec).unwrap_or_default(),
    ]
}

/// A single follow-mode line for a record.
fn follow_line(rec: &jsonl_reader::Record) -> String {
    record_cells(rec).join("  ")
}

/// The SUMMARY cell: the rendered message for internal events, or a compact
/// payload (`params` when present) for protocol events. `None` when there is
/// nothing useful to show (the row still renders, with an empty summary).
fn summary(rec: &jsonl_reader::Record) -> Option<String> {
    if rec.kind == "internal" {
        return rec.message.clone().filter(|m| !m.is_empty());
    }
    let payload = rec.payload.as_ref()?;
    let target = payload.get("params").unwrap_or(payload);
    Some(clip(&target.to_string(), 160))
}

/// Format an RFC3339 timestamp as a local `HH:MM:SS`, falling back to the raw
/// string when it does not parse.
fn local_hms(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts).map_or_else(
        |_| ts.to_string(),
        |dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string(),
    )
}

/// Char-safe clip to at most `max` characters, appending `...` when truncated.
/// Operates on `char` boundaries so multi-byte payloads never panic.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let kept: String = s.chars().take(max - 3).collect();
    format!("{kept}...")
}

/// Garbage-collect old session data.
///
/// Deletes events, dead sessions, or specific sessions based on the flags.
/// Runs `VACUUM` when significant data is removed.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or a delete fails.
pub fn run_gc(
    out: &mut Output,
    conn: &rusqlite::Connection,
    older_than: Option<&str>,
    dead: bool,
    session_id: Option<&str>,
) -> Result<()> {
    let mut total_events_deleted: usize = 0;
    let mut sessions_deleted: usize = 0;
    // --older-than: delete old events and filter history
    if let Some(duration_str) = older_than {
        let cutoff = parse_since(duration_str)?;
        let cutoff_str = cutoff.to_rfc3339();
        let events_removed =
            conn.execute("DELETE FROM events WHERE timestamp < ?1", [&cutoff_str])?;
        total_events_deleted += events_removed;

        let filters_removed = conn.execute(
            "DELETE FROM filter_history WHERE created_at < ?1",
            [&cutoff_str],
        )?;

        let _ = out.writeln(format_args!(
            "Deleted {events_removed} event{}{} older than {duration_str}",
            if events_removed == 1 { "" } else { "s" },
            if filters_removed > 0 {
                format!(", {filters_removed} filter history entries")
            } else {
                String::new()
            },
        ));
    }

    // --dead: detect crashed sessions, then delete all dead sessions
    if dead {
        // Mark crashed sessions (alive in DB but PID gone)
        let crashed: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id, pid FROM sessions WHERE alive = 1")?;
            let mut rows = stmt.query([])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let pid: u32 = row.get(1)?;
                if !session::is_process_alive(pid) {
                    ids.push(id);
                }
            }
            ids
        };

        let ended_at = Utc::now().to_rfc3339();
        for id in &crashed {
            let _ = conn.execute(
                "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                rusqlite::params![&ended_at, id],
            );
        }

        // Count events that will be cascade-deleted
        let dead_events: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id IN \
                 (SELECT id FROM sessions WHERE alive = 0)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let removed = conn.execute("DELETE FROM sessions WHERE alive = 0", [])?;
        sessions_deleted += removed;
        total_events_deleted += dead_events;

        let _ = out.writeln(format_args!(
            "Deleted {removed} dead session{} ({dead_events} event{})",
            if removed == 1 { "" } else { "s" },
            if dead_events == 1 { "" } else { "s" },
        ));
    }

    // --session: delete a specific session
    if let Some(sid) = session_id {
        let resolved = resolve_session_id(conn, sid)?;

        let event_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                [&resolved.id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        conn.execute("DELETE FROM sessions WHERE id = ?1", [&resolved.id])?;
        sessions_deleted += 1;
        total_events_deleted += event_count;

        // Clean up socket directory
        let socket_dir = session::sessions_dir().join(&resolved.id);
        let _ = std::fs::remove_dir_all(&socket_dir);

        let _ = out.writeln(format_args!(
            "Deleted session {} ({event_count} event{})",
            &resolved.id[..8.min(resolved.id.len())],
            if event_count == 1 { "" } else { "s" },
        ));
    }

    if older_than.is_none() && !dead && session_id.is_none() {
        let _ = out.writeln(format_args!(
            "Nothing to do. Use --older-than, --dead, or --session."
        ));
    }

    // VACUUM if significant data was deleted
    if total_events_deleted > 1000 || sessions_deleted > 0 {
        let size_before = db_file_size();
        conn.execute_batch("VACUUM")?;
        let size_after = db_file_size();

        if let (Some(before), Some(after)) = (size_before, size_after) {
            let saved = before.saturating_sub(after);
            if saved > 0 {
                let _ = out.writeln(format_args!(
                    "Database vacuumed (saved {})",
                    format_bytes(saved)
                ));
            }
        }
    }

    Ok(())
}

/// Get the database file size in bytes.
fn db_file_size() -> Option<u64> {
    std::fs::metadata(crate::db::db_path())
        .ok()
        .map(|m| m.len())
}

/// Format a byte count as a human-readable string.
#[allow(
    clippy::cast_precision_loss,
    reason = "byte counts are small enough for f64"
)]
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Find session by ID or prefix.
///
/// # Errors
///
/// Returns an error if the session cannot be found.
pub fn find_session(conn: &rusqlite::Connection, id: &str) -> Result<session::SessionInfo> {
    // Try exact match first
    if let Some((s, _)) = session::get_session_with_conn(conn, id)? {
        return Ok(s);
    }

    // Try prefix match on both internal ID and client session ID
    let sessions = session::list_sessions_with_conn(conn)?;
    let matches: Vec<_> = sessions
        .iter()
        .filter(|(s, _)| {
            s.id.starts_with(id)
                || s.client_session_id
                    .as_deref()
                    .is_some_and(|csid| csid.starts_with(id))
        })
        .collect();

    match matches.len() {
        0 => anyhow::bail!("No session found matching '{id}'"),
        1 => Ok(matches[0].0.clone()),
        _ => {
            eprintln!("Multiple sessions match '{id}':");
            for (s, _) in matches {
                eprintln!("  {}", s.id);
            }
            anyhow::bail!("Please specify a more complete session ID")
        }
    }
}

/// Format a timestamp as "Xm ago" or similar.
#[must_use]
pub fn format_duration_ago(timestamp: chrono::DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(timestamp);

    if duration.num_hours() > 0 {
        format!(
            "{}h {}m ago",
            duration.num_hours(),
            duration.num_minutes() % 60
        )
    } else if duration.num_minutes() > 0 {
        format!("{}m ago", duration.num_minutes())
    } else {
        format!("{}s ago", duration.num_seconds())
    }
}

/// Print a message in raw JSON format.
fn print_message_raw(out: &mut Output, msg: &SessionMessage) {
    let time = msg.timestamp.with_timezone(&Local).format("%H:%M:%S");
    let tag = match msg.r#type.as_str() {
        "lsp" => format!("[{}]", msg.server),
        "mcp" => "[mcp]".to_string(),
        "hook" => "[hook]".to_string(),
        other => format!("[{other}]"),
    };
    let arrow = if msg.payload.get("result").is_some() || msg.payload.get("error").is_some() {
        "\u{2190}" // ←
    } else {
        "\u{2192}" // →
    };
    let _ = out.writeln(format_args!("[{time}] {tag} {arrow}"));
    let pretty = serde_json::to_string_pretty(&msg.payload).unwrap_or_default();
    let _ = out.writeln(format_args!("{pretty}"));
}

/// Print a message with annotations and colors.
#[allow(clippy::too_many_lines, reason = "match arms for each message type")]
fn print_message_annotated(out: &mut Output, msg: &SessionMessage) {
    let time = msg.timestamp.with_timezone(&Local).format("%H:%M:%S");
    let time_str = out.colors.dim(&format!("[{time}]"));

    let is_response = msg.payload.get("result").is_some() || msg.payload.get("error").is_some();

    match msg.r#type.as_str() {
        "lsp" => {
            let lang = out.colors.cyan(&msg.server);
            if msg.method == "$/progress" {
                let title = msg
                    .payload
                    .get("value")
                    .and_then(|v| v.get("title"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let pct = msg
                    .payload
                    .get("value")
                    .and_then(|v| v.get("percentage"))
                    .and_then(serde_json::Value::as_u64)
                    .map(|p| format!(" {p}%"))
                    .unwrap_or_default();
                let detail = msg
                    .payload
                    .get("value")
                    .and_then(|v| v.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default();
                let _ = out.writeln(format_args!("{time_str} {lang}: {title}{pct}{detail}"));
            } else if is_response {
                let arrow = out.colors.blue("\u{2190}");
                if msg.payload.get("error").is_some() {
                    let err_msg = msg
                        .payload
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown error");
                    let _ = out.writeln(format_args!(
                        "{time_str} {lang} {arrow} {} {}",
                        msg.method,
                        out.colors.red(err_msg)
                    ));
                } else {
                    let _ = out.writeln(format_args!("{time_str} {lang} {arrow} {}", msg.method));
                }
            } else {
                let arrow = out.colors.green("\u{2192}");
                let _ = out.writeln(format_args!("{time_str} {lang} {arrow} {}", msg.method));
            }
        }
        "mcp" => {
            let summary = extract_message_summary(&msg.payload, &out.colors);
            let prefix_len = 10 + 5 + 2 + 2; // [time] + [mcp] + arrow + spaces
            let max_summary_len = out.width.saturating_sub(prefix_len);

            if is_response {
                let arrow = out.colors.blue("\u{2190}");
                let summary = cli::truncate(&summary, max_summary_len);
                let _ = out.writeln(format_args!("{time_str} [mcp] {arrow} {summary}"));
            } else {
                let arrow = out.colors.green("\u{2192}");
                let summary = cli::truncate(&summary, max_summary_len);
                let _ = out.writeln(format_args!("{time_str} [mcp] {arrow} {summary}"));
            }

            if let Some(error) = msg.payload.get("error") {
                let err_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                let _ = out.writeln(format_args!(
                    "    {}",
                    out.colors.red(&format!("Error: {err_msg}"))
                ));
            }
        }
        "hook" => {
            if let Some(count_val) = msg.payload.get("count") {
                let count = count_val.as_u64().unwrap_or(0);
                let file = msg
                    .payload
                    .get("file")
                    .and_then(|f| f.as_str())
                    .unwrap_or(&msg.method);
                let basename = std::path::Path::new(file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(file);

                if count == 0 {
                    let check = out.colors.green("ok");
                    let _ = out.writeln(format_args!("{time_str} {basename}: {check}"));
                } else {
                    let label = out.colors.yellow(&format!(
                        "{count} diagnostic{}",
                        if count == 1 { "" } else { "s" }
                    ));
                    let preview = msg
                        .payload
                        .get("preview")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let detail = if preview.is_empty() {
                        String::new()
                    } else {
                        let max_len = out.width.saturating_sub(14 + basename.len() + 20);
                        format!(" -- {}", cli::truncate(preview, max_len))
                    };
                    let _ = out.writeln(format_args!("{time_str} {basename}: {label}{detail}"));
                }
            } else {
                let _ = out.writeln(format_args!("{time_str} [hook] {}", msg.method));
            }
        }
        other => {
            let _ = out.writeln(format_args!("{time_str} [{other}] {}", msg.method));
        }
    }
}

/// Extract a human-readable summary from a JSON-RPC payload.
fn extract_message_summary(payload: &serde_json::Value, colors: &ColorConfig) -> String {
    let Some(obj) = payload.as_object() else {
        return payload.to_string();
    };

    obj.get("method").and_then(|m| m.as_str()).map_or_else(
        || {
            if obj.contains_key("result") || obj.contains_key("error") {
                let id = obj.get("id").map(|i| format!("#{i}")).unwrap_or_default();
                if obj.contains_key("error") {
                    format!("{} {}", colors.red("error"), id)
                } else {
                    format!("result {id}")
                }
            } else {
                serde_json::to_string(payload).unwrap_or_default()
            }
        },
        |method| {
            let id = obj.get("id").map(|i| format!("#{i}")).unwrap_or_default();
            let params_summary = match method {
                "tools/call" => {
                    if let Some(params) = obj.get("params")
                        && let Some(name) = params.get("name").and_then(|n| n.as_str())
                    {
                        let file_info = params
                            .get("arguments")
                            .and_then(|a| a.get("file_path").or_else(|| a.get("path")))
                            .and_then(|f| f.as_str())
                            .map(|f| {
                                std::path::Path::new(f)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(f)
                            })
                            .map(|f| format!(" ({f})"))
                            .unwrap_or_default();
                        format!("{}{}", colors.cyan(name), file_info)
                    } else {
                        String::new()
                    }
                }
                "initialize" => {
                    if let Some(params) = obj.get("params")
                        && let Some(info) = params.get("clientInfo")
                        && let Some(name) = info.get("name").and_then(|n| n.as_str())
                    {
                        format!("from {name}")
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            };

            if params_summary.is_empty() {
                format!("{method} {id}")
            } else {
                format!("{method} {params_summary} {id}")
            }
        },
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::session;

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test DB");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("failed to open test DB");
        (dir, path, conn)
    }

    /// Insert a dead session directly into the database.
    fn insert_dead_session(conn: &rusqlite::Connection, id: &str, workspace: &str) {
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, alive, ended_at) \
             VALUES (?1, 0, ?2, ?3, 0, ?4)",
            rusqlite::params![
                id,
                workspace,
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:01:00Z",
            ],
        )
        .expect("insert dead session");
    }

    // ── parse_since tests ────────────────────────────────────────────

    #[test]
    fn test_parse_since_hours() -> anyhow::Result<()> {
        let cutoff = parse_since("1h")?;
        let diff = Utc::now().signed_duration_since(cutoff);
        // Should be approximately 1 hour (allow 5s tolerance)
        assert!(
            diff.num_seconds() >= 3595 && diff.num_seconds() <= 3605,
            "expected ~3600s, got {}s",
            diff.num_seconds()
        );
        Ok(())
    }

    #[test]
    fn test_parse_since_days() -> anyhow::Result<()> {
        let cutoff = parse_since("7d")?;
        let diff = Utc::now().signed_duration_since(cutoff);
        let expected = 7 * 86400;
        assert!(
            diff.num_seconds() >= expected - 5 && diff.num_seconds() <= expected + 5,
            "expected ~{expected}s, got {}s",
            diff.num_seconds()
        );
        Ok(())
    }

    #[test]
    fn test_parse_since_minutes() -> anyhow::Result<()> {
        let cutoff = parse_since("30m")?;
        let diff = Utc::now().signed_duration_since(cutoff);
        assert!(
            diff.num_seconds() >= 1795 && diff.num_seconds() <= 1805,
            "expected ~1800s, got {}s",
            diff.num_seconds()
        );
        Ok(())
    }

    #[test]
    fn test_parse_since_today() -> anyhow::Result<()> {
        let cutoff = parse_since("today")?;
        let now = Utc::now();
        // Cutoff should be before now
        assert!(cutoff <= now);
        // And within the last 24 hours
        assert!(now.signed_duration_since(cutoff).num_hours() < 24);
        Ok(())
    }

    #[test]
    fn test_parse_since_invalid() {
        assert!(parse_since("abc").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("5x").is_err());
    }

    // ── query rendering tests ───────────────────────────────────────
    //
    // The read path (file selection, filtering, ordering) is unit-tested in
    // `jsonl_reader`; these cover the `commands.rs` raw rendering layer.

    /// A protocol record for rendering assertions.
    fn proto_record(method: &str) -> jsonl_reader::Record {
        jsonl_reader::Record {
            ts: "2026-06-09T10:11:12.000Z".into(),
            kind: "lsp".into(),
            level: "info".into(),
            scope_id: "rust-analyzer@/p".into(),
            parent_id: Some("p-1".into()),
            server: "rust-analyzer".into(),
            scope_root: String::new(),
            cwd: String::new(),
            method: method.into(),
            source: None,
            payload: Some(serde_json::json!({"id": 1, "method": method, "params": {"q": "x"}})),
            message: None,
            language: None,
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn table_render_has_headers_and_a_row() {
        let mut out = Output::buffer(200);
        render_rows(
            &mut out,
            &[proto_record("textDocument/hover")],
            QueryFormat::Table,
        );
        let text = out.into_string();
        assert!(text.contains("METHOD"), "header present: {text}");
        assert!(text.contains("textDocument/hover"), "row rendered: {text}");
    }

    #[test]
    fn table_render_empty_says_no_results() {
        let mut out = Output::buffer(80);
        render_rows(&mut out, &[], QueryFormat::Table);
        assert!(out.into_string().contains("No results"));
    }

    #[test]
    fn json_render_emits_raw_records_no_adornments() {
        let mut out = Output::buffer(200);
        render_rows(&mut out, &[proto_record("tools/call")], QueryFormat::Json);
        let v: serde_json::Value =
            serde_json::from_str(&out.into_string()).expect("valid json array");
        let first = &v[0];
        // Raw firehose record — no merge adornments.
        assert_eq!(first["method"], "tools/call");
        assert_eq!(first["payload"]["params"]["q"], "x");
        assert!(first.get("outcome").is_none(), "no merge outcome: {first}");
        assert!(first.get("count").is_none(), "no collapse count: {first}");
        // Empty keys omitted (matches on-disk shape).
        assert!(
            first.get("scope_root").is_none(),
            "empty key omitted: {first}"
        );
    }

    #[test]
    fn json_render_empty_is_empty_array() {
        let mut out = Output::buffer(80);
        render_rows(&mut out, &[], QueryFormat::Json);
        let v: serde_json::Value = serde_json::from_str(&out.into_string()).expect("json");
        assert_eq!(v, serde_json::json!([]));
    }

    #[test]
    fn summary_prefers_params_for_protocol_records() {
        let cells = record_cells(&proto_record("tools/call"));
        let summary = cells.last().expect("summary cell");
        assert!(summary.contains("\"q\""), "shows params: {summary}");
    }

    #[test]
    fn summary_uses_message_for_internal_records() {
        let mut rec = proto_record("crate::mod");
        rec.kind = "internal".into();
        rec.payload = None;
        rec.message = Some("rust-analyzer exited".into());
        let cells = record_cells(&rec);
        assert_eq!(
            cells.last().map(String::as_str),
            Some("rust-analyzer exited")
        );
    }

    #[test]
    fn clip_is_char_safe_on_multibyte() {
        // Must not panic on a non-ASCII boundary, and must shorten.
        let s = "héllo wörld with áccénts everywhere indeed";
        let out = clip(s, 10);
        assert!(out.chars().count() <= 10);
        assert!(out.ends_with("..."));
    }

    // ── gc tests ────────────────────────────────────────────────────

    #[test]
    fn test_gc_dead_sessions() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "gc-dead-1", "/tmp/test-gc-dead");

        // Verify it exists as dead
        let found = session::get_session_with_conn(&conn, "gc-dead-1")?;
        assert!(found.is_some(), "session should exist");
        assert!(!found.expect("checked above").1, "session should be dead");

        // Run gc --dead
        let mut out = Output::buffer(120);
        run_gc(&mut out, &conn, None, true, None)?;

        // Should be gone
        assert!(
            session::get_session_with_conn(&conn, "gc-dead-1")?.is_none(),
            "dead session should be deleted"
        );
        Ok(())
    }

    #[test]
    fn test_gc_specific_session() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "gc-s1", "/tmp/test-gc-specific-1");
        insert_dead_session(&conn, "gc-s2", "/tmp/test-gc-specific-2");

        // Delete only s1
        let mut out = Output::buffer(120);
        run_gc(&mut out, &conn, None, false, Some("gc-s1"))?;

        assert!(
            session::get_session_with_conn(&conn, "gc-s1")?.is_none(),
            "targeted session should be deleted"
        );
        assert!(
            session::get_session_with_conn(&conn, "gc-s2")?.is_some(),
            "other session should survive"
        );

        session::delete_session_data_with_conn(&conn, "gc-s2")?;
        Ok(())
    }

    #[test]
    fn test_gc_no_flags_is_noop() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        // Should not error
        let mut out = Output::buffer(120);
        run_gc(&mut out, &conn, None, false, None)?;
        Ok(())
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2_621_440), "2.5 MB");
    }

    // ── resolve_session_id tests ────────────────────────────────────

    #[test]
    fn resolve_session_id_by_row_number() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "resolve-row-1", "/tmp/ws1");
        insert_dead_session(&conn, "resolve-row-2", "/tmp/ws2");

        // Row 1 should return the first session
        let s = resolve_session_id(&conn, "1")?;
        assert_eq!(s.id, "resolve-row-1");

        // Row 2 should return the second session
        let s = resolve_session_id(&conn, "2")?;
        assert_eq!(s.id, "resolve-row-2");

        Ok(())
    }

    #[test]
    fn resolve_session_id_row_zero_falls_through() {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "resolve-zero", "/tmp/ws");

        // Row 0 is invalid — should not match any session.
        // "0" could also be an ID prefix, but no session starts with "0".
        let result = resolve_session_id(&conn, "0");
        assert!(result.is_err(), "row 0 should not match");
    }

    #[test]
    fn resolve_session_id_row_out_of_range() {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "resolve-oor", "/tmp/ws");

        // Row 99 is out of range and not a valid ID prefix.
        let result = resolve_session_id(&conn, "99");
        assert!(result.is_err(), "out-of-range row should error");
    }

    #[test]
    fn resolve_session_id_by_prefix() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "abc123", "/tmp/ws");

        let s = resolve_session_id(&conn, "abc")?;
        assert_eq!(s.id, "abc123");
        Ok(())
    }

    #[test]
    fn resolve_session_id_numeric_id_fallback() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        // Session ID that looks numeric but is larger than the row count.
        insert_dead_session(&conn, "999def", "/tmp/ws");

        // "999" as a row number would be out of range (only 1 session).
        // But "999" as an ID prefix should match "999def".
        let s = resolve_session_id(&conn, "999")?;
        assert_eq!(s.id, "999def");
        Ok(())
    }

    // ── find_session tests ──────────────────────────────────────────

    #[test]
    fn find_session_exact_match() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "exact-match-id", "/tmp/ws");

        let s = find_session(&conn, "exact-match-id")?;
        assert_eq!(s.id, "exact-match-id");
        Ok(())
    }

    #[test]
    fn find_session_prefix_match() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "prefix-abc123", "/tmp/ws");

        let s = find_session(&conn, "prefix-abc")?;
        assert_eq!(s.id, "prefix-abc123");
        Ok(())
    }

    #[test]
    fn find_session_no_match() {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "some-session", "/tmp/ws");

        let err = find_session(&conn, "nonexistent").expect_err("should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("No session found"),
            "error should say 'No session found', got: {msg}",
        );
    }

    #[test]
    fn find_session_ambiguous_match() {
        let (_dir, _path, conn) = test_db();
        insert_dead_session(&conn, "ambig-001", "/tmp/ws1");
        insert_dead_session(&conn, "ambig-002", "/tmp/ws2");

        // "ambig" matches both — should error with disambiguation message.
        let err = find_session(&conn, "ambig").expect_err("should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("more complete session ID"),
            "error should ask for more specific ID, got: {msg}",
        );
    }

    // ── format_duration_ago tests ───────────────────────────────────

    #[test]
    fn format_duration_ago_hours() {
        let ts = Utc::now() - chrono::Duration::hours(2) - chrono::Duration::minutes(15);
        let result = format_duration_ago(ts);
        assert!(result.contains("2h"), "should show hours, got: {result}");
        assert!(
            result.contains("15m"),
            "should show remaining minutes, got: {result}",
        );
        assert!(result.ends_with("ago"), "should end with 'ago'");
    }

    #[test]
    fn format_duration_ago_minutes_only() {
        let ts = Utc::now() - chrono::Duration::minutes(5);
        let result = format_duration_ago(ts);
        assert!(result.contains("5m"), "should show minutes, got: {result}");
        assert!(
            !result.contains('h'),
            "should NOT show hours for <1h, got: {result}",
        );
        assert!(result.ends_with("ago"));
    }

    #[test]
    fn format_duration_ago_seconds_only() {
        let ts = Utc::now() - chrono::Duration::seconds(30);
        let result = format_duration_ago(ts);
        assert!(result.contains("30s"), "should show seconds, got: {result}");
        assert!(
            !result.contains('m'),
            "should NOT show minutes for <1m, got: {result}",
        );
        assert!(result.ends_with("ago"));
    }

    // ── extract_message_summary tests ───────────────────────────────

    #[test]
    fn extract_summary_with_method() {
        let colors = ColorConfig::new(true);
        let payload = serde_json::json!({
            "method": "textDocument/hover",
            "id": 5
        });
        let result = extract_message_summary(&payload, &colors);
        assert!(
            result.contains("textDocument/hover"),
            "should contain method, got: {result}",
        );
        assert!(
            result.contains("#5"),
            "should contain request id, got: {result}"
        );
    }

    #[test]
    fn extract_summary_result_without_method() {
        let colors = ColorConfig::new(true);
        let payload = serde_json::json!({ "result": null, "id": 3 });
        let result = extract_message_summary(&payload, &colors);
        assert!(
            result.contains("result"),
            "should indicate result, got: {result}",
        );
    }

    #[test]
    fn extract_summary_error_without_method() {
        let colors = ColorConfig::new(true);
        let payload = serde_json::json!({ "error": { "code": -1 }, "id": 7 });
        let result = extract_message_summary(&payload, &colors);
        assert!(
            result.contains("error"),
            "should indicate error, got: {result}",
        );
    }

    #[test]
    fn extract_summary_tools_call_with_name() {
        let colors = ColorConfig::new(true);
        let payload = serde_json::json!({
            "method": "tools/call",
            "id": 1,
            "params": {
                "name": "grep",
                "arguments": { "file_path": "/src/main.rs" }
            }
        });
        let result = extract_message_summary(&payload, &colors);
        assert!(
            result.contains("grep"),
            "should contain tool name, got: {result}",
        );
        assert!(
            result.contains("main.rs"),
            "should contain file basename, got: {result}",
        );
    }
}
