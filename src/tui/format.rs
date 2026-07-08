// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Low-level time and layout helpers shared by the grid's renderers.
//!
//! Time-in-state ([`elapsed_short`]) is the primitive that makes a stuck
//! `probing` server visible — a steadily growing value. Durations are
//! **quantized** ([`format_elapsed_secs`]) to coarse buckets so an idle board
//! stays byte-identical between refreshes (tui-rework 11, item 4 — idle engine,
//! idle board). Layout helpers ([`truncate_to_width`], [`bound_line`],
//! [`justify`]) keep every rendered line within its area width, truncating with
//! `…` rather than clipping raw (item 1).

use chrono::{DateTime, Local, Utc};
use ratatui::style::Style;
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
///
/// Reads the wall clock; the render layer uses the injected-clock twin
/// [`seconds_between`] so a board renders deterministically from a fixed `now`.
#[must_use]
pub fn seconds_since(iso: &str) -> Option<i64> {
    seconds_between(iso, Utc::now())
}

/// Seconds elapsed from `since` until `now` (non-negative), or `None` if
/// `since` is unparseable — the injected-clock freshness primitive.
///
/// The render layer computes every duration against a single injected `now`, so
/// two refreshes inside the same quantization bucket produce byte-identical
/// output (tui-rework 11, item 4).
#[must_use]
pub fn seconds_between(since: &str, now: DateTime<Utc>) -> Option<i64> {
    parse_iso(since).map(|start| (now - start).num_seconds().max(0))
}

/// Compact elapsed time from `since` until now, quantized (see
/// [`format_elapsed_secs`]). Returns an empty string if `since` is unparseable.
///
/// Reads the wall clock; the render layer uses the injected-clock twin
/// [`elapsed_at`].
#[must_use]
pub fn elapsed_short(since: &str) -> String {
    seconds_since(since).map_or_else(String::new, format_elapsed_secs)
}

/// Quantized compact elapsed from `since` until `now`; empty on unparseable
/// input. The render-layer twin of [`elapsed_short`].
#[must_use]
pub fn elapsed_at(since: &str, now: DateTime<Utc>) -> String {
    seconds_between(since, now).map_or_else(String::new, format_elapsed_secs)
}

/// Format a non-negative second count as a **quantized** compact duration.
///
/// Coarse buckets so an idle row stops ticking (tui-rework 11, item 4): seconds
/// only under a minute (`42s`), then whole minutes (`7m`, never `7m25s`), then
/// `1h05m` under a day, then whole days (`2d`). A healthy row is then
/// byte-identical between refreshes for minutes at a time.
#[must_use]
pub fn format_elapsed_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Footer freshness phrase for a snapshot age (tui-rework 11, item 4).
///
/// `just now` under ~30s, then `<1m ago`, then the quantized `Nm ago` / `1h05m
/// ago` / `Nd ago`. Reads naturally after the word `updated`.
#[must_use]
pub fn format_freshness(secs: i64) -> String {
    if secs < 30 {
        "just now".to_string()
    } else if secs < 60 {
        "<1m ago".to_string()
    } else {
        format!("{} ago", format_elapsed_secs(secs))
    }
}

// ── Layout helpers ───────────────────────────────────────────────────

/// Total display width of a span sequence.
#[must_use]
pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Take the leading `cols` display columns of `s` (no ellipsis), Unicode-honest.
fn take_cols(s: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.to_string().width();
        if w + cw > cols {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
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
    let mut out = take_cols(s, max.saturating_sub(1));
    out.push('…');
    out
}

/// Bound a whole line to `width` display columns, marking any cut with `…`.
///
/// The safety net every rendered line passes through so nothing clips raw at its
/// pane edge (tui-rework 11, item 1); the `…` inherits the trailing span's
/// style. Unicode-honest: measured and cut by display width, not bytes.
#[must_use]
pub fn bound_line(line: &Line<'static>, width: usize) -> Line<'static> {
    if spans_width(&line.spans) <= width {
        return line.clone();
    }
    Line::from(truncate_spans(line.spans.clone(), width))
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

/// Truncate a span sequence to `max` display columns, marking any drop with `…`.
///
/// Preserves each span's style and appends a single `…` (in the trailing span's
/// style) whenever content is dropped. The one truncation primitive the grid
/// shares — `justify` truncates its left column through it, and [`bound_line`]
/// bounds whole lines.
#[must_use]
pub fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max {
        return spans;
    }
    if max == 0 {
        return Vec::new();
    }
    // Content must be dropped; reserve one column for the ellipsis.
    let budget = max - 1;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let mut ellipsis_style = Style::default();
    for span in spans {
        let w = span.content.width();
        if used + w <= budget {
            used += w;
            ellipsis_style = span.style;
            out.push(span);
        } else {
            let remaining = budget - used;
            if remaining > 0 {
                ellipsis_style = span.style;
                out.push(Span::styled(
                    take_cols(&span.content, remaining),
                    span.style,
                ));
            }
            break;
        }
    }
    out.push(Span::styled("…".to_string(), ellipsis_style));
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn format_elapsed_buckets_quantize_at_boundaries() {
        // Under a minute: seconds resolution.
        assert_eq!(format_elapsed_secs(0), "0s");
        assert_eq!(format_elapsed_secs(59), "59s");
        // A minute and up: whole minutes only — no ticking seconds.
        assert_eq!(format_elapsed_secs(60), "1m");
        assert_eq!(format_elapsed_secs(72), "1m");
        assert_eq!(format_elapsed_secs(119), "1m");
        assert_eq!(format_elapsed_secs(7 * 60 + 25), "7m");
        // An hour and up: `NhMMm`.
        assert_eq!(format_elapsed_secs(3600), "1h00m");
        assert_eq!(format_elapsed_secs(3 * 3600 + 5 * 60), "3h05m");
        // A day and up: whole days.
        assert_eq!(format_elapsed_secs(86_400), "1d");
        assert_eq!(format_elapsed_secs(2 * 86_400 + 3600), "2d");
    }

    #[test]
    fn freshness_buckets() {
        assert_eq!(format_freshness(0), "just now");
        assert_eq!(format_freshness(29), "just now");
        assert_eq!(format_freshness(30), "<1m ago");
        assert_eq!(format_freshness(59), "<1m ago");
        assert_eq!(format_freshness(60), "1m ago");
        assert_eq!(format_freshness(4 * 60 + 21), "4m ago");
    }

    #[test]
    fn elapsed_at_is_deterministic_within_a_bucket() {
        let start = "2026-07-07T12:00:00Z";
        let base = parse_iso("2026-07-07T12:01:40Z").expect("iso");
        // 100s and 110s elapsed both floor to the same whole minute.
        assert_eq!(elapsed_at(start, base), "1m");
        assert_eq!(
            elapsed_at(start, base + chrono::Duration::seconds(10)),
            "1m"
        );
    }

    #[test]
    fn elapsed_short_handles_garbage() {
        assert_eq!(elapsed_short("not-a-date"), "");
        assert!(seconds_since("not-a-date").is_none());
        assert!(seconds_between("not-a-date", Utc::now()).is_none());
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate_to_width("hello world", 5), "hell…");
        assert_eq!(truncate_to_width("hi", 5), "hi");
    }

    #[test]
    fn bound_line_never_exceeds_width_and_marks_truncation() {
        // A line wider than the bound truncates to width with a trailing `…`.
        let line = Line::from(vec![
            Span::styled("daemon pid 4242".to_string(), Style::new()),
            Span::styled(" · updated just now".to_string(), Style::new()),
        ]);
        let bounded = bound_line(&line, 10);
        assert_eq!(spans_width(&bounded.spans), 10, "bounded to exactly width");
        let text: String = bounded.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with('…'), "carries the ellipsis: {text}");
        // A line that already fits is returned unchanged (no ellipsis).
        let fits = bound_line(&line, 100);
        let fits_text: String = fits.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!fits_text.contains('…'), "no ellipsis when it fits");
    }

    #[test]
    fn truncate_spans_marks_drop_at_a_span_boundary() {
        // Two full-width spans, bound to the first span's width: the second is
        // dropped, so the result must still carry an ellipsis.
        let spans = vec![
            Span::styled("AAAAA".to_string(), Style::new()),
            Span::styled("BBBBB".to_string(), Style::new()),
        ];
        let out = truncate_spans(spans, 5);
        assert!(spans_width(&out) <= 5, "within bound");
        let text: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('…'), "boundary drop is marked: {text}");
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
