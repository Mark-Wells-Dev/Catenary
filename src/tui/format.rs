// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Row formatters for the dashboard boards.
//!
//! Each board entry is turned into one or more styled [`Line`]s here; the panel
//! renderers in [`super`] only lay out blocks, scroll, and highlight. The
//! firehose-rendering pipeline (pair-merge, scope-collapse, summarize) is gone
//! (observability ticket 06) — those transforms now live in `catenary query`,
//! not the TUI.

use chrono::{DateTime, Local, Utc};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::icons::{IconSet, basename};
use super::theme::Theme;
use crate::state_snapshot::{Alert, ServerEntry, SessionEntry, SessionStatus};

/// Lines rendered per server board entry (fixed, so the board can map an entry
/// index to a line range for cursor highlight + scroll).
pub const SERVER_ENTRY_LINES: usize = 2;
/// Lines rendered per session board entry.
pub const SESSION_ENTRY_LINES: usize = 2;

// ── Time helpers ─────────────────────────────────────────────────────

/// Parse an ISO 8601 timestamp, returning `None` on any malformed input.
fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Local wall-clock `HH:MM:SS` for an ISO timestamp (empty string on failure).
///
/// Timestamps in `state.json` are UTC; the dashboard shows them in local time
/// (the ws25 "timestamps display in UTC" fix).
#[must_use]
pub fn local_hms(iso: &str) -> String {
    parse_iso(iso).map_or_else(String::new, |dt| {
        dt.with_timezone(&Local).format("%H:%M:%S").to_string()
    })
}

/// Compact elapsed time from `since` until now, e.g. `3m12s`, `2h05m`, `4d01h`.
///
/// This is the **time-in-state** primitive: a server stuck in `probing` shows a
/// steadily growing value, which is exactly the ws25 "stuck initializing" bug
/// made visible. Returns an empty string if `since` is unparseable.
#[must_use]
pub fn elapsed_short(since: &str) -> String {
    let Some(start) = parse_iso(since) else {
        return String::new();
    };
    let secs = (Utc::now() - start).num_seconds().max(0);
    format_elapsed_secs(secs)
}

/// Format a non-negative second count as a compact duration.
#[must_use]
fn format_elapsed_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

// ── Layout helpers ───────────────────────────────────────────────────

/// Total display width of a span sequence.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Truncate a string to `max` display columns, appending `…` when it was cut.
#[must_use]
pub fn truncate_to_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.to_string().width();
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Build a line with `left` packed left and `right` flushed right within
/// `width`, padding the gap. When the two would collide, `left` is truncated so
/// `right` (the at-a-glance status / time) stays visible.
fn justify(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let right_w = spans_width(&right);
    let mut left_w = spans_width(&left);

    // Reserve at least one space between left and right; truncate left if needed.
    if left_w + right_w + 1 > width && !left.is_empty() {
        let max_left = width.saturating_sub(right_w + 1);
        left = truncate_spans(left, max_left);
        left_w = spans_width(&left);
    }

    let gap = width.saturating_sub(left_w + right_w);
    let mut spans = left;
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.extend(right);
    Line::from(spans)
}

/// Truncate a span sequence to `max` columns, preserving each span's style and
/// appending `…` to the last surviving span when content was dropped.
fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = span.content.width();
        if used + w <= max {
            used += w;
            out.push(span);
        } else {
            let remaining = max.saturating_sub(used);
            if remaining > 0 {
                let style = span.style;
                out.push(Span::styled(
                    truncate_to_width(&span.content, remaining),
                    style,
                ));
            }
            break;
        }
    }
    out
}

// ── Server board ─────────────────────────────────────────────────────

/// Style + label for a server's lifecycle state.
fn state_label(state: &str, busy_count: Option<u32>, theme: &Theme) -> (String, Style) {
    match state {
        "healthy" => ("healthy".to_string(), theme.success),
        "busy" => (
            busy_count.map_or_else(|| "busy".to_string(), |n| format!("busy({n})")),
            theme.accent,
        ),
        "initializing" => ("initializing".to_string(), theme.warning),
        "probing" => ("probing".to_string(), theme.warning),
        "failed" => ("failed".to_string(), theme.error),
        "dead" => ("dead".to_string(), theme.muted),
        other => (other.to_string(), theme.text),
    }
}

/// Render one server board entry as [`SERVER_ENTRY_LINES`] styled lines.
///
/// Line 1: `<server>` (left) · `<state>` (right, state-colored).
/// Line 2 (dim): `<root> · <progress|last message>` (left) · time-in-state
/// (right) — a stuck `probing` shows a growing time-in-state.
#[must_use]
pub fn server_entry_lines(
    e: &ServerEntry,
    width: usize,
    theme: &Theme,
    icons: &IconSet,
) -> Vec<Line<'static>> {
    let (label, label_style) = state_label(&e.state, e.busy_count, theme);

    let dot = match e.state.as_str() {
        "healthy" | "busy" => icons.ls_active.clone(),
        _ => icons.ls_inactive.clone(),
    };
    let line1 = justify(
        vec![
            Span::styled(dot, label_style),
            Span::styled(e.server.clone(), theme.text),
        ],
        vec![Span::styled(label, label_style)],
        width,
    );

    // Sub-line: root context + progress or last message, with time-in-state.
    let mut detail = if e.scope_root.is_empty() {
        e.scope_kind.clone()
    } else {
        basename(&e.scope_root).to_string()
    };
    let extra = e.progress.as_ref().map_or_else(
        || e.last_message.as_ref().map(|m| m.text.replace('\n', " ")),
        |p| {
            let pct = p.pct.map_or_else(String::new, |v| format!("{v}% "));
            let msg = p
                .message
                .as_deref()
                .map_or_else(String::new, |m| format!(" {m}"));
            Some(format!("{pct}{}{msg}", p.title))
        },
    );
    if let Some(extra) = extra {
        let extra = extra.trim();
        if !extra.is_empty() {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(extra);
        }
    }

    let time = elapsed_short(&e.state_since);
    let line2 = justify(
        vec![Span::styled(format!("  {detail}"), theme.muted)],
        if time.is_empty() {
            vec![]
        } else {
            vec![Span::styled(time, theme.muted)]
        },
        width,
    );

    vec![line1, line2]
}

// ── Session board ────────────────────────────────────────────────────

/// Style + label for a session status.
const fn session_status_label(status: SessionStatus, theme: &Theme) -> (&'static str, Style) {
    match status {
        SessionStatus::Editing => ("editing", theme.accent),
        SessionStatus::Diagnostics => ("diagnostics", theme.warning),
        SessionStatus::Idle => ("idle", theme.muted),
    }
}

/// Render one session board entry as [`SESSION_ENTRY_LINES`] styled lines.
///
/// Line 1: `<client>` (left) · `<status>` (right, status-colored).
/// Line 2 (dim): `<last action | roots>` (left) · `seen <recency>` (right) —
/// `last_seen` is the liveness signal a cold session lacks a death event for
/// (ticket 05a), distinct from `last_action` (ticket 05).
#[must_use]
pub fn session_entry_lines(
    e: &SessionEntry,
    width: usize,
    theme: &Theme,
    icons: &IconSet,
) -> Vec<Line<'static>> {
    let (status, status_style) = session_status_label(e.status, theme);
    let dot = if matches!(e.status, SessionStatus::Idle) {
        icons.session_shutdown.clone()
    } else {
        icons.session_started.clone()
    };
    let client = if e.client.name.is_empty() {
        "unknown".to_string()
    } else {
        e.client.name.clone()
    };
    let line1 = justify(
        vec![
            Span::styled(dot, status_style),
            Span::styled(client, theme.text),
        ],
        vec![Span::styled(status.to_string(), status_style)],
        width,
    );

    // Sub-line: last action (preferred) or workspace roots, plus recency.
    let detail = e.last_action.as_ref().map_or_else(
        || {
            let names: Vec<String> = e.roots.iter().map(|r| basename(r).to_string()).collect();
            names.join(", ")
        },
        |a| a.summary.replace('\n', " "),
    );
    let recency = elapsed_short(&e.last_seen);
    let right = if recency.is_empty() {
        vec![]
    } else {
        vec![Span::styled(format!("seen {recency}"), theme.muted)]
    };
    let line2 = justify(
        vec![Span::styled(format!("  {detail}"), theme.muted)],
        right,
        width,
    );

    vec![line1, line2]
}

// ── Alerts ring ──────────────────────────────────────────────────────

/// Render one alert as a single line: `<icon> <time> <text> (<scope>)`.
///
/// Errors and warnings are color-coded; the time is local wall-clock. The
/// scope (when present) is the yankable bridge into `catenary query`.
#[must_use]
pub fn alert_line(a: &Alert, width: usize, theme: &Theme, icons: &IconSet) -> Line<'static> {
    let (icon, style) = if a.level == "error" {
        (icons.diag_error.clone(), theme.error)
    } else {
        (icons.diag_warn.clone(), theme.warning)
    };
    let time = local_hms(&a.at);
    let mut text = a.text.replace('\n', " ");
    if let Some(scope) = a.scope.as_deref().filter(|s| !s.is_empty()) {
        text.push_str(" (");
        text.push_str(scope);
        text.push(')');
    }

    let prefix: Vec<Span<'static>> = vec![
        Span::styled(icon, style),
        Span::styled(format!("{time} "), theme.timestamp),
    ];
    let prefix_w = spans_width(&prefix);
    let body = truncate_to_width(&text, width.saturating_sub(prefix_w));
    let mut spans = prefix;
    spans.push(Span::styled(body, theme.text));
    Line::from(spans)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::state_snapshot::{ClientInfo, LastAction, LastMessage, Progress};
    use crate::tui::icons::IconSet;

    fn icons() -> IconSet {
        IconSet::from_config(crate::config::IconConfig::default())
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn format_elapsed_buckets() {
        assert_eq!(format_elapsed_secs(5), "5s");
        assert_eq!(format_elapsed_secs(72), "1m12s");
        assert_eq!(format_elapsed_secs(3 * 3600 + 5 * 60), "3h05m");
        assert_eq!(format_elapsed_secs(2 * 86_400 + 3600), "2d01h");
    }

    #[test]
    fn elapsed_short_handles_garbage() {
        assert_eq!(elapsed_short("not-a-date"), "");
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate_to_width("hello world", 5), "hell…");
        assert_eq!(truncate_to_width("hi", 5), "hi");
    }

    #[test]
    fn server_line_shows_state_and_time_in_state() {
        let theme = Theme::new();
        let e = ServerEntry {
            id: "rust-analyzer@/p/Catenary".to_string(),
            server: "rust-analyzer".to_string(),
            scope_root: "/p/Catenary".to_string(),
            state: "probing".to_string(),
            // 5m05s ago → time-in-state visible.
            state_since: (Utc::now() - chrono::Duration::seconds(305))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            progress: Some(Progress {
                title: "Indexing".to_string(),
                message: Some("src/db.rs".to_string()),
                pct: Some(62),
            }),
            ..ServerEntry::default()
        };
        let lines = server_entry_lines(&e, 40, &theme, &icons());
        assert_eq!(lines.len(), SERVER_ENTRY_LINES);
        let l0 = line_text(&lines[0]);
        let l1 = line_text(&lines[1]);
        assert!(l0.contains("rust-analyzer"), "{l0}");
        assert!(l0.contains("probing"), "{l0}");
        assert!(l1.contains("5m05s"), "time-in-state: {l1}");
        assert!(l1.contains("62% Indexing"), "progress: {l1}");
        assert!(l1.contains("Catenary"), "root basename: {l1}");
    }

    #[test]
    fn server_line_shows_busy_count() {
        let theme = Theme::new();
        let e = ServerEntry {
            server: "ra".to_string(),
            state: "busy".to_string(),
            busy_count: Some(3),
            state_since: Utc::now().to_rfc3339(),
            ..ServerEntry::default()
        };
        let lines = server_entry_lines(&e, 40, &theme, &icons());
        assert!(line_text(&lines[0]).contains("busy(3)"));
    }

    #[test]
    fn server_line_falls_back_to_last_message() {
        let theme = Theme::new();
        let e = ServerEntry {
            server: "ra".to_string(),
            state: "failed".to_string(),
            state_since: Utc::now().to_rfc3339(),
            last_message: Some(LastMessage {
                level: "error".to_string(),
                text: "Failed to load workspace".to_string(),
                at: Utc::now().to_rfc3339(),
            }),
            ..ServerEntry::default()
        };
        let lines = server_entry_lines(&e, 50, &theme, &icons());
        assert!(line_text(&lines[1]).contains("Failed to load workspace"));
    }

    #[test]
    fn session_line_shows_status_and_action() {
        let theme = Theme::new();
        let e = SessionEntry {
            id: "mcp:7f3a".to_string(),
            client: ClientInfo {
                name: "claude".to_string(),
                version: None,
            },
            status: SessionStatus::Editing,
            last_seen: (Utc::now() - chrono::Duration::seconds(12)).to_rfc3339(),
            last_action: Some(LastAction {
                summary: "edited src/db.rs".to_string(),
                at: Utc::now().to_rfc3339(),
            }),
            roots: vec!["/p/Catenary".to_string()],
            ..SessionEntry::default()
        };
        let lines = session_entry_lines(&e, 40, &theme, &icons());
        assert_eq!(lines.len(), SESSION_ENTRY_LINES);
        assert!(line_text(&lines[0]).contains("claude"));
        assert!(line_text(&lines[0]).contains("editing"));
        assert!(line_text(&lines[1]).contains("edited src/db.rs"));
        assert!(line_text(&lines[1]).contains("seen 12s"));
    }

    #[test]
    fn session_line_unknown_client_and_roots_fallback() {
        let theme = Theme::new();
        let e = SessionEntry {
            id: "s".to_string(),
            status: SessionStatus::Idle,
            roots: vec!["/p/A".to_string(), "/p/B".to_string()],
            ..SessionEntry::default()
        };
        let lines = session_entry_lines(&e, 40, &theme, &icons());
        assert!(line_text(&lines[0]).contains("unknown"));
        assert!(line_text(&lines[0]).contains("idle"));
        assert!(line_text(&lines[1]).contains('A'));
        assert!(line_text(&lines[1]).contains('B'));
    }

    #[test]
    fn alert_line_color_codes_and_includes_scope() {
        let theme = Theme::new();
        let a = Alert {
            at: "2026-06-08T14:32:00.000Z".to_string(),
            level: "error".to_string(),
            source: Some("lsp".to_string()),
            text: "rust-analyzer exited (code 101)".to_string(),
            scope: Some("rust-analyzer@/p/Catenary".to_string()),
        };
        let line = alert_line(&a, 80, &theme, &icons());
        let t = line_text(&line);
        assert!(t.contains("rust-analyzer exited"), "{t}");
        assert!(t.contains("(rust-analyzer@/p/Catenary)"), "scope: {t}");
    }
}
