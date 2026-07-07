// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Low-level time and layout helpers shared by the grid's renderers.
//!
//! Time-in-state ([`elapsed_short`]) is the primitive that makes a stuck
//! `probing` server visible — a steadily growing value. Layout helpers
//! ([`truncate_to_width`], [`justify`]) keep a right-flushed status column
//! visible when a row is too narrow (filter before columns).

use chrono::{DateTime, Local, Utc};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

// ── Time helpers ─────────────────────────────────────────────────────

/// Parse an ISO 8601 timestamp, returning `None` on any malformed input.
#[must_use]
pub fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Local wall-clock `HH:MM:SS` for an ISO timestamp (empty string on failure).
#[must_use]
pub fn local_hms(iso: &str) -> String {
    parse_iso(iso).map_or_else(String::new, |dt| {
        dt.with_timezone(&Local).format("%H:%M:%S").to_string()
    })
}

/// Seconds elapsed since `iso` until now (non-negative), or `None` if
/// unparseable — the freshness/staleness primitive.
#[must_use]
pub fn seconds_since(iso: &str) -> Option<i64> {
    parse_iso(iso).map(|start| (Utc::now() - start).num_seconds().max(0))
}

/// Compact elapsed time from `since` until now, e.g. `3m12s`, `2h05m`, `4d01h`.
///
/// The **time-in-state** primitive: a server stuck in `probing` shows a
/// steadily growing value. Returns an empty string if `since` is unparseable.
#[must_use]
pub fn elapsed_short(since: &str) -> String {
    seconds_since(since).map_or_else(String::new, format_elapsed_secs)
}

/// Format a non-negative second count as a compact duration.
#[must_use]
pub fn format_elapsed_secs(secs: i64) -> String {
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
#[must_use]
pub fn spans_width(spans: &[Span<'_>]) -> usize {
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

/// Build a line with `left` packed left and `right` flushed right.
///
/// The gap is padded to `width`. When the two would collide, `left` is
/// truncated so `right` (the at-a-glance status / time) stays visible — filter
/// before columns.
#[must_use]
pub fn justify(
    mut left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let right_w = spans_width(&right);
    let mut left_w = spans_width(&left);

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
#[must_use]
pub fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

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
        assert!(seconds_since("not-a-date").is_none());
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate_to_width("hello world", 5), "hell…");
        assert_eq!(truncate_to_width("hi", 5), "hi");
    }

    #[test]
    fn justify_truncates_left_to_keep_right_visible() {
        let line = justify(
            vec![Span::styled(
                "a very long left label indeed".to_string(),
                Style::new(),
            )],
            vec![Span::styled("RIGHT".to_string(), Style::new())],
            20,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("RIGHT"), "right column survives: {text}");
        assert!(text.contains('…'), "left is truncated: {text}");
    }
}
