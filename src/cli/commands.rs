// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CLI subcommands: query, ls-roots, and commands.

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use std::time::Duration;

use crate::cli::{Output, QueryFormat, jsonl_reader};

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

        // Distinguish the classes (ephemeral-roots ticket 02): an ephemeral,
        // activity-mounted root shows `[ephemeral · expires when idle]` instead
        // of its raw `ephemeral:*` contributor key; a pinned root keeps its
        // contributor sources.
        let ephemeral = entry
            .get("ephemeral")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let label = if ephemeral {
            "ephemeral \u{b7} expires when idle".to_string()
        } else if sources.is_empty() {
            "unknown".to_string()
        } else {
            sources.join(", ")
        };

        let _ = out.writeln(format_args!(
            "{path}  {}",
            out.colors.dim(&format!("[{label}]"))
        ));
    }

    Ok(())
}

/// Resolve a `pin`/`unpin` path to the form matched against the daemon's
/// tracked set.
///
/// `catenary pin` / `catenary unpin` match the stored/normalized path.
/// Canonicalize when the directory exists (so a symlinked spelling resolves to
/// the tracked form the daemon stored at pin time); when it does not — an
/// `unpin` after the directory was removed (bug 54 gap 1) — fall back to the
/// lexically-absolutized spelling rather than hard-erroring, so a deleted-dir
/// pin is still removable. Never fails: a `pin` targets a real directory
/// (canonicalization succeeds), while an `unpin` must tolerate a vanished one.
#[must_use]
pub fn resolve_root_path(path: &std::path::Path) -> std::path::PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Interpret the daemon's response to a `roots-add` (pin) / `roots-rm` (unpin)
/// request for the pin/unpin CLI surface.
///
/// `Ok(line)` is printed to stdout; `Err(msg)` bails the command. A `not_found`
/// on unpin is a benign, idempotent no-op — the pin contributor was already
/// absent (a repeat `unpin`, or a path that was never pinned) — so it is
/// reported as success rather than an error. `is_pin` selects the verb; only
/// unpin can see `not_found` (a pin targets a live directory).
///
/// # Errors
///
/// Returns `Err(message)` when the daemon reports a failure (any status other
/// than `ok`/`not_found`), carrying the daemon's message when present.
pub fn render_root_outcome(
    status: &str,
    is_pin: bool,
    path: &str,
    daemon_msg: Option<&str>,
) -> Result<String, String> {
    match status {
        "ok" if is_pin => Ok(format!("pinned root: {path}")),
        "ok" => Ok(format!("unpinned root: {path}")),
        // Idempotent unpin: the pin contributor was already gone. Not an error.
        "not_found" => Ok(format!("not pinned (nothing to unpin): {path}")),
        _ => Err(daemon_msg.unwrap_or("unexpected response").to_string()),
    }
}

/// The writes half of the write-model line closing the active-allowlist
/// `catenary commands` output (ws38 / decision 026). The lists above govern
/// which programs may *run*; this states how their *writes* are judged —
/// resolve-or-deny. The navigation half ([`WRITE_MODEL_NAVIGATE`]) names the
/// navigation tools in capability voice (teaching-surface ticket 13); both
/// halves are config-free, so the surface never asserts a denial the resolved
/// config may not make.
const WRITE_MODEL_RESOLVE: &str = "Writes resolve-or-deny: a redirect or a \
     `cp`/`mv`/`tee`/`sed -i`/`git apply` write is allowed when its complete \
     target set resolves (recorded for `catenary diagnostics`) and denied with a \
     teaching message when it can't.";

/// The navigation half of the write-model line — the config-free capability
/// pointer naming Catenary's navigation tools (teaching-surface ticket 13).
///
/// The former teach-07 clause asserted which native scanners "stay denied"; the
/// maintainer ruled that policy assertion out of the primer (a user may enable
/// native `grep`/`find`), so this states the capability instead. It is
/// config-independent and always present, so no surface claims a denial a user's
/// config may not make. The enrichment behavior and the stdin pass are stated in
/// full by the invariants (`crate::cli::teaching`).
const WRITE_MODEL_NAVIGATE: &str = "`catenary grep` and `catenary glob` are the navigation tools.";

/// Assemble the write-model line for the active command surface.
///
/// Two config-free halves: [`WRITE_MODEL_RESOLVE`] states how writes are judged
/// (resolve-or-deny) and [`WRITE_MODEL_NAVIGATE`] names the navigation tools in
/// capability voice (teaching-surface ticket 13). Neither half is config-derived,
/// so the line states capabilities the session always has, never a
/// config-specific denial.
fn write_model_line() -> String {
    format!("{WRITE_MODEL_RESOLVE} {WRITE_MODEL_NAVIGATE}")
}

/// Render the `catenary commands` output lines for a resolved command set and
/// the build tool(s) resolved for the current directory.
///
/// Mirrors the states the `PreToolUse` hook distinguishes: a deliberate
/// opt-out (`client_enforcement_only`), an active allowlist (the cwd build
/// tool plus the surface sections), or no `[commands]` section at all. Pure —
/// the IO (config load + cwd build resolution) lives in
/// [`resolve_commands_for_cwd`] — so the branching is unit-testable.
/// `build_tools` is the already-resolved set for the cwd (empty when none
/// applies).
///
/// Shared with the teaching payload (`crate::cli::teaching`) so its live
/// commands-surface tier is the *same* rendering `catenary commands` prints.
pub(crate) fn render_command_lines(
    resolved: Option<&crate::config::ResolvedCommands>,
    build_tools: &[String],
) -> Vec<String> {
    match resolved {
        Some(r) if r.client_enforcement_only => vec![
            "Catenary command enforcement is disabled (client_enforcement_only); the \
             host CLI's own permissions apply."
                .to_string(),
        ],
        Some(r) if r.is_active() => {
            let mut lines = Vec::new();
            // Lead with the build tool: an agent running `catenary commands`
            // eagerly learns the cwd's build tool here rather than only on a
            // build-command denial.
            if !build_tools.is_empty() {
                lines.push(format!("Build tool: {}", build_tools.join(", ")));
            }
            lines.extend(crate::cli::command_filter::format_command_surface(r));
            if lines.is_empty() {
                vec!["No build tool, allow, pipeline, or deny rules are configured.".to_string()]
            } else {
                // Close with the write model (ws38 / decision 026): writes are
                // resolve-or-deny, and the navigation tools are named in
                // capability voice — both halves config-free, so the surface
                // never asserts a denial the config may not make (teaching-surface
                // ticket 13).
                lines.push(write_model_line());
                lines
            }
        }
        Some(_) | None => vec![
            "No [commands] section is configured — Catenary does not filter shell commands."
                .to_string(),
        ],
    }
}

/// Resolve the live command surface for the current working directory.
///
/// Loads the user-level command-filter config — the same `[commands]` surface
/// the `PreToolUse` hook enforces — and merges the nearest `.catenary.toml`'s
/// per-root build tool for the current directory (reusing the same
/// [`find_project_config`](crate::cli::hooks::find_project_config) walk as the
/// client-side denial path, so the build tool resolved here matches the denial
/// hint). Returns the resolved `[commands]` table (if any) and the build
/// tool(s) that apply to the cwd.
///
/// Shared by [`run_commands`] and the teaching payload
/// (`crate::cli::teaching`), so both project the *same* runtime surface.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded or parsed.
pub(crate) fn resolve_commands_for_cwd()
-> Result<(Option<crate::config::ResolvedCommands>, Vec<String>)> {
    let config = crate::config::Config::load().context("load Catenary configuration")?;
    let cwd = std::env::current_dir().ok();

    // Resolve the cwd's effective build tool the way the client-side hook does:
    // user config + the nearest `.catenary.toml`'s per-root `build`.
    let resolved = config.resolved_commands.map(|mut r| {
        if let Some(ref cwd_path) = cwd
            && let Some((root, pc)) = crate::cli::hooks::find_project_config(cwd_path)
        {
            let mut project_commands = std::collections::HashMap::new();
            if let Some(cmds) = pc.commands {
                project_commands.insert(root.clone(), cmds);
            }
            r = r.merge_project_commands(std::slice::from_ref(&root), &project_commands);
        }
        r
    });

    let build_tools: Vec<String> = resolved
        .as_ref()
        .filter(|r| r.is_active())
        .map(|r| r.build_for_cwd(cwd.as_deref()).to_vec())
        .unwrap_or_default();

    Ok((resolved, build_tools))
}

/// Print the active allowed-command surface for the current configuration.
///
/// Resolves the live surface via [`resolve_commands_for_cwd`] and prints the
/// cwd build tool plus the allow / pipeline / denied sections. Stateless: no
/// daemon connection, matching `catenary doctor`'s command-filter check.
/// Denial messages point the agent here so the full surface lives in one place
/// instead of being dumped inline on every first denial.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded or parsed.
pub fn run_commands(out: &mut Output) -> Result<()> {
    let (resolved, build_tools) = resolve_commands_for_cwd()?;
    for line in render_command_lines(resolved.as_ref(), &build_tools) {
        let _ = out.writeln(format_args!("{line}"));
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
///
/// Field naming is the on-disk firehose contract, not the table headers
/// (bug 125): the timestamp key is `ts` (RFC3339 millis, UTC) — there is no
/// `time` alias — and there is no `session` key: the session id rides in
/// `scope_id`, which is self-describing (session id / search UUID /
/// `server@root` / instance id). A record with no value for an optional key
/// omits the key entirely — never an empty string masquerading as data — so a
/// jq extraction that comes back empty means the key name is wrong, not that
/// the field went unpopulated.
fn render_json(out: &mut Output, records: &[jsonl_reader::Record]) {
    let json = serde_json::to_string_pretty(records).unwrap_or_default();
    let _ = out.writeln(format_args!("{json}"));
}

/// The seven table cells for one record.
fn record_cells(rec: &jsonl_reader::Record) -> Vec<String> {
    vec![
        local_iso(&rec.ts),
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

/// Format an RFC3339 timestamp as a local, date-bearing ISO-8601
/// `YYYY-MM-DDTHH:MM:SS`, falling back to the raw string when it does not
/// parse.
///
/// Always date-bearing, not window-conditional (bug 125): a time-of-day-only
/// cell turns ambiguous the moment a query window crosses midnight
/// (`--since 7d`) — exactly the forensic moment this surface exists for.
/// Grep-friendly and unambiguous beats compact.
fn local_iso(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts).map_or_else(
        |_| ts.to_string(),
        |dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        },
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── pin / unpin path + outcome (misc 146, bug 54) ────────────────

    #[test]
    fn resolve_root_path_canonicalizes_an_existing_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_root_path(dir.path()),
            dir.path()
                .canonicalize()
                .expect("canonicalize existing dir"),
        );
    }

    #[test]
    fn resolve_root_path_falls_back_for_a_missing_dir() {
        // bug 54 gap 1: `unpin` after the directory is gone must still resolve a
        // matchable path — never hard-error on a failed canonicalize. The
        // fallback is the lexically-absolutized spelling, which matches the
        // canonical form the daemon stored at pin time (no symlink components).
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("removed-worktree"); // never created on disk
        assert_eq!(
            resolve_root_path(&gone),
            std::path::absolute(&gone).expect("absolutize"),
        );
    }

    #[test]
    fn pin_and_unpin_report_their_verbs() {
        assert!(
            render_root_outcome("ok", true, "/p", None)
                .expect("pin ok")
                .starts_with("pinned"),
        );
        assert!(
            render_root_outcome("ok", false, "/p", None)
                .expect("unpin ok")
                .starts_with("unpinned"),
        );
    }

    #[test]
    fn unpin_of_an_absent_pin_is_idempotent_success() {
        // A repeat `unpin` (pin already dropped) or an unpin of a never-pinned
        // path succeeds — never errors, so the command is idempotent.
        let outcome = render_root_outcome("not_found", false, "/gone", None);
        let line = outcome.expect("not_found is a benign success for unpin");
        assert!(line.contains("/gone"), "{line}");
    }

    #[test]
    fn root_outcome_bails_on_daemon_error() {
        let err =
            render_root_outcome("error", true, "/p", Some("boom")).expect_err("error status bails");
        assert_eq!(err, "boom");
        // Missing message falls back to a generic reason.
        let err =
            render_root_outcome("weird", false, "/p", None).expect_err("unknown status bails");
        assert_eq!(err, "unexpected response");
    }

    // ── render_command_lines tests ───────────────────────────────────

    #[test]
    fn commands_active_lists_surface() {
        let resolved = crate::config::ResolvedCommands {
            allow: std::collections::HashSet::from(["git".into(), "make".into()]),
            pipeline: std::collections::HashSet::from(["grep".into()]),
            ..crate::config::ResolvedCommands::default()
        };
        let joined = render_command_lines(Some(&resolved), &[]).join("\n");
        assert!(joined.contains("Allowed: git, make"), "{joined}");
        assert!(
            joined.contains("Allowed in pipelines (not first): grep"),
            "{joined}",
        );
        // No build tool resolved for the cwd → no build line.
        assert!(!joined.contains("Build tool:"), "{joined}");
        // The active surface closes with the resolve-or-deny write model.
        assert!(joined.contains("Writes resolve-or-deny"), "{joined}");
    }

    #[test]
    fn commands_active_leads_with_build_tool() {
        let resolved = crate::config::ResolvedCommands {
            allow: std::collections::HashSet::from(["git".into()]),
            ..crate::config::ResolvedCommands::default()
        };
        let lines = render_command_lines(Some(&resolved), &["make".to_string()]);
        assert_eq!(lines.first().map(String::as_str), Some("Build tool: make"));
        assert!(
            lines.iter().any(|l| l.contains("Allowed: git")),
            "{lines:?}"
        );
    }

    #[test]
    fn commands_client_enforcement_only() {
        let resolved = crate::config::ResolvedCommands {
            client_enforcement_only: true,
            ..crate::config::ResolvedCommands::default()
        };
        let lines = render_command_lines(Some(&resolved), &[]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("client_enforcement_only"), "{lines:?}");
    }

    #[test]
    fn commands_absent_section() {
        let lines = render_command_lines(None, &[]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("No [commands] section"), "{lines:?}");
    }

    #[test]
    fn commands_build_only_shows_build_line() {
        // Active via a build tool, no allow / pipeline / deny rules: the build
        // line plus the closing write-model line.
        let resolved = crate::config::ResolvedCommands {
            default_build: vec!["make".into()],
            ..crate::config::ResolvedCommands::default()
        };
        assert!(resolved.is_active());
        let lines = render_command_lines(Some(&resolved), &["make".to_string()]);
        assert_eq!(lines.first().map(String::as_str), Some("Build tool: make"));
        assert!(
            lines
                .last()
                .is_some_and(|l| l.contains("Writes resolve-or-deny")),
            "{lines:?}",
        );
    }

    #[test]
    fn commands_active_but_nothing_to_show() {
        // Defensive: active (per-root build map populated) yet no surface and no
        // cwd build resolved → a single explanatory line, never empty output.
        let resolved = crate::config::ResolvedCommands {
            build: std::collections::HashMap::from([(
                std::path::PathBuf::from("/elsewhere"),
                vec!["make".into()],
            )]),
            ..crate::config::ResolvedCommands::default()
        };
        assert!(resolved.is_active());
        let lines = render_command_lines(Some(&resolved), &[]);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("No build tool, allow, pipeline, or deny rules"),
            "{lines:?}",
        );
    }

    #[test]
    fn project_build_override_resolves_for_cwd() {
        // Mirrors `run_commands`' resolution: the nearest `.catenary.toml`'s
        // build overrides the user default for a cwd inside that root.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".catenary.toml"),
            "[commands]\nbuild = \"ninja\"\n",
        )
        .expect("write project config");

        let user = crate::config::ResolvedCommands {
            allow: std::collections::HashSet::from(["git".into()]),
            default_build: vec!["make".into()],
            ..crate::config::ResolvedCommands::default()
        };

        let (root, pc) =
            crate::cli::hooks::find_project_config(dir.path()).expect("project config found");
        let mut project_commands = std::collections::HashMap::new();
        if let Some(cmds) = pc.commands {
            project_commands.insert(root.clone(), cmds);
        }
        let merged = user.merge_project_commands(std::slice::from_ref(&root), &project_commands);

        assert_eq!(
            merged.build_for_cwd(Some(dir.path())),
            ["ninja".to_string()],
            "project build should override the user default for the cwd",
        );
    }

    // ── write-model navigation capability (teaching-surface 13) ──────

    #[test]
    fn write_model_names_the_navigation_tools_in_capability_voice() {
        // teaching-surface 13: the write-model line's navigation half states a
        // capability — grep/glob are the navigation tools — replacing the former
        // teach-07 clause that asserted which native scanners "stay denied". Both
        // halves are config-free, so the exact line is fixed.
        assert_eq!(
            write_model_line(),
            format!("{WRITE_MODEL_RESOLVE} {WRITE_MODEL_NAVIGATE}"),
        );
        assert_eq!(
            WRITE_MODEL_NAVIGATE,
            "`catenary grep` and `catenary glob` are the navigation tools.",
        );
    }

    #[test]
    fn write_model_asserts_no_navigation_denial() {
        // The maintainer's ruling (teach 13): the primer states capabilities, not
        // policy — the navigation half must never claim navigation "stays denied"
        // or that native scanners "bypass" anything. (The resolve-or-deny half
        // legitimately says a *write* is "denied when it can't" resolve; that is
        // the write model, not a navigation-policy assertion.)
        let line = write_model_line();
        for forbidden in ["stays denied", "bypass", "code intelligence"] {
            assert!(
                !line.contains(forbidden),
                "write-model line asserts navigation policy ({forbidden:?}): {line}"
            );
        }
        assert!(
            line.contains("Writes resolve-or-deny"),
            "resolve-or-deny half stays: {line}"
        );
        assert!(
            line.contains("`catenary grep` and `catenary glob`"),
            "navigation tools named: {line}"
        );
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

    // ── bug 125: date-bearing table timestamps ──────────────────────

    #[test]
    fn table_timestamp_is_date_bearing_iso8601() {
        // bug 125: a `HH:MM:SS`-only cell is ambiguous across a multi-day
        // window (`--since 7d`). The TIME cell must carry the date — always,
        // not window-conditionally. Expected value computed through the same
        // local conversion so the assertion is timezone-independent.
        let ts = "2026-06-09T10:11:12.000Z";
        let expected = chrono::DateTime::parse_from_rfc3339(ts)
            .expect("valid rfc3339")
            .with_timezone(&Local)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert_eq!(local_iso(ts), expected);

        let cells = record_cells(&proto_record("textDocument/hover"));
        assert_eq!(
            cells.first().map(String::as_str),
            Some(expected.as_str()),
            "TIME cell carries the date"
        );
    }

    #[test]
    fn table_timestamp_falls_back_to_raw_when_unparseable() {
        assert_eq!(local_iso("not-a-timestamp"), "not-a-timestamp");
    }

    // ── bug 125: JSON field naming/population ───────────────────────

    #[test]
    fn json_rows_carry_ts_and_scope_id_never_empty_strings() {
        // bug 125: the JSON shape is the on-disk firehose contract — the
        // timestamp key is `ts` (no `time` alias) and the session id rides in
        // `scope_id` (no `session` key). Every row carries `ts`; a row class
        // with no scope omits `scope_id` entirely per the emitter's
        // skip-empty convention — never an empty string masquerading as data.
        let with_scope = proto_record("tools/call"); // scope_id = server@root
        let mut scopeless = proto_record("initialize");
        scopeless.scope_id = String::new(); // e.g. a daemon-level event

        let mut out = Output::buffer(400);
        render_rows(&mut out, &[with_scope, scopeless], QueryFormat::Json);
        let v: serde_json::Value =
            serde_json::from_str(&out.into_string()).expect("valid json array");

        for row in v.as_array().expect("array") {
            assert!(
                row.get("ts").and_then(serde_json::Value::as_str)
                    == Some("2026-06-09T10:11:12.000Z"),
                "every row carries a populated ts: {row}"
            );
            assert!(row.get("time").is_none(), "no `time` alias: {row}");
            assert!(row.get("session").is_none(), "no `session` key: {row}");
        }
        assert_eq!(
            v[0]["scope_id"], "rust-analyzer@/p",
            "scoped row's scope_id populated"
        );
        assert!(
            v[1].get("scope_id").is_none(),
            "scopeless row omits the key rather than emitting \"\": {}",
            v[1]
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
}
