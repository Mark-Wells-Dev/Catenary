// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Context-sensitive navigation hints bar rendered at the bottom of the TUI.
//!
//! Hints change based on which region has keyboard focus: sidebar
//! (sessions/servers) or stream. Uses width degradation to fit in
//! narrow terminals.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::app::FocusRegion;
use super::theme::Theme;

/// Hints shown when the sidebar (sessions or servers) is focused.
const SIDEBAR_HINTS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("Tab", "focus"),
    ("Space", "toggle"),
    ("j/k", "navigate"),
    ("b", "sidebar"),
];

/// Hints shown when the message stream is focused.
const STREAM_HINTS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("Tab", "focus"),
    ("Enter", "expand"),
    ("j/k", "navigate"),
    ("b", "sidebar"),
    ("y", "yank"),
];

/// Select hints for the current focus region.
const fn hints_for_focus(focus: FocusRegion) -> &'static [(&'static str, &'static str)] {
    match focus {
        FocusRegion::Sessions | FocusRegion::Servers => SIDEBAR_HINTS,
        FocusRegion::Stream => STREAM_HINTS,
    }
}

/// Return the navigation hints that fit in the given width.
///
/// Progressively drops hints from the front until they fit.
#[must_use]
fn degrade_hints(
    all: &[(&'static str, &'static str)],
    max_width: u16,
) -> Vec<(&'static str, &'static str)> {
    let max = max_width as usize;

    // Level 1: all hints with separators.
    if hints_width_with_separators(all) <= max {
        return all.to_vec();
    }

    // Level 2: all hints, space-separated.
    if hints_width_spaced(all) <= max {
        return all.to_vec();
    }

    // Levels 3+: progressively drop hints from the front.
    for drop_count in 1..all.len() {
        let remaining = &all[drop_count..];
        if hints_width_spaced(remaining) <= max {
            return remaining.to_vec();
        }
    }

    // Empty.
    Vec::new()
}

/// Render context-sensitive navigation hints into a 1-row area.
///
/// Hints change based on `focus`. Rendered between styled border caps:
/// `──┤ hints ├──` (light).
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_hints(area: Rect, buf: &mut Buffer, theme: &Theme, focus: FocusRegion) {
    if area.width < 4 || area.height < 1 {
        return;
    }

    let hint_budget = area.width.saturating_sub(6);
    let all = hints_for_focus(focus);
    let hints = degrade_hints(all, hint_budget);

    if hints.is_empty() {
        render_border_only(area, buf, theme);
        return;
    }

    // Build hint spans.
    let total_width_with_seps = hints_width_with_separators(&hints);
    let total_width_spaced = hints_width_spaced(&hints);
    let use_separators = total_width_with_seps <= hint_budget as usize;

    let mut hint_spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            if use_separators {
                hint_spans.push(Span::styled(" \u{2571} ", theme.muted)); // ╱
            } else {
                hint_spans.push(Span::raw(" "));
            }
        }
        hint_spans.push(Span::styled((*key).to_string(), theme.hint_key));
        if !label.is_empty() {
            hint_spans.push(Span::raw(" "));
            hint_spans.push(Span::styled((*label).to_string(), theme.hint_label));
        }
    }

    let hints_text_width = if use_separators {
        total_width_with_seps
    } else {
        total_width_spaced
    };

    // Border characters.
    let h_line = "\u{2500}"; // ─
    let left_cap = "\u{2524}"; // ┤
    let right_cap = "\u{251C}"; // ├

    // Fill pattern: left_fill, left_cap, space, hints, space, right_cap, right_fill
    let inner_used = 1 + 1 + hints_text_width + 1 + 1; // left_cap, space, hints, space, right_cap
    let fill_total = (area.width as usize).saturating_sub(inner_used);
    let fill_right = fill_total / 2;
    let fill_left = fill_total.saturating_sub(fill_right);

    let mut spans: Vec<Span<'static>> = Vec::new();

    if fill_left > 0 {
        spans.push(Span::styled(
            h_line.repeat(fill_left),
            theme.border_unfocused,
        ));
    }

    spans.push(Span::styled(left_cap.to_string(), theme.border_unfocused));
    spans.push(Span::raw(" "));
    spans.extend(hint_spans);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(right_cap.to_string(), theme.border_unfocused));

    if fill_right > 0 {
        spans.push(Span::styled(
            h_line.repeat(fill_right),
            theme.border_unfocused,
        ));
    }

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}

/// Render just the border line when no hints fit.
fn render_border_only(area: Rect, buf: &mut Buffer, theme: &Theme) {
    let h_line = "\u{2500}"; // ─
    let fill = area.width as usize;
    if fill > 0 {
        let line = Line::from(Span::styled(h_line.repeat(fill), theme.border_unfocused));
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

/// Display width of a single hint: `key label` or just `key`.
fn hint_display_width(key: &str, label: &str) -> usize {
    if label.is_empty() {
        UnicodeWidthStr::width(key)
    } else {
        UnicodeWidthStr::width(key) + 1 + UnicodeWidthStr::width(label)
    }
}

/// Total display width of hints joined by ` ╱ ` separators.
fn hints_width_with_separators(hints: &[(&str, &str)]) -> usize {
    if hints.is_empty() {
        return 0;
    }
    let content: usize = hints.iter().map(|(k, s)| hint_display_width(k, s)).sum();
    let seps = (hints.len() - 1) * 3;
    content + seps
}

/// Total display width of hints joined by single spaces.
fn hints_width_spaced(hints: &[(&str, &str)]) -> usize {
    if hints.is_empty() {
        return 0;
    }
    let content: usize = hints.iter().map(|(k, s)| hint_display_width(k, s)).sum();
    content + hints.len() - 1
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Convert a ratatui buffer to a single string for assertion matching.
    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                s.push_str(cell.symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn test_stream_hints_render_full() {
        let theme = Theme::new();
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, FocusRegion::Stream);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains('q'), "expected 'q' hint key in: {content}");
        assert!(
            content.contains("yank"),
            "expected 'yank' hint in stream mode: {content}"
        );
        assert!(
            content.contains("expand"),
            "expected 'expand' hint in stream mode: {content}"
        );
    }

    #[test]
    fn test_sidebar_hints_render_full() {
        let theme = Theme::new();
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, FocusRegion::Sessions);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains('q'), "expected 'q' hint key in: {content}");
        assert!(
            content.contains("toggle"),
            "expected 'toggle' hint in sidebar mode: {content}"
        );
        // Sidebar should NOT have yank or expand.
        assert!(
            !content.contains("yank"),
            "sidebar should not show yank: {content}"
        );
    }

    #[test]
    fn test_hints_render_narrow_border_only() {
        let theme = Theme::new();
        let backend = TestBackend::new(5, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, FocusRegion::Stream);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            !content.contains('q'),
            "no hints at narrow width: {content}"
        );
        assert!(
            content.contains('\u{2500}'),
            "should have border line: {content}"
        );
    }

    #[test]
    fn test_hints_render_too_narrow() {
        let theme = Theme::new();
        let backend = TestBackend::new(3, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, FocusRegion::Stream);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            !content.contains('\u{2500}'),
            "width < 4 should render nothing: {content}"
        );
    }

    #[test]
    fn test_no_debug_toggle_hint() {
        let theme = Theme::new();
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, FocusRegion::Stream);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            !content.contains("debug"),
            "debug toggle should not appear: {content}"
        );
    }

    // ── Width calculation tests ─────────────────────────────────────────

    #[test]
    fn test_hints_width_with_separators() {
        assert_eq!(hints_width_with_separators(&[]), 0);
        // "q quit" = 6 cols
        let one: Vec<(&str, &str)> = vec![("q", "quit")];
        assert_eq!(hints_width_with_separators(&one), 6);
        // "q quit" + " ╱ " + "b sidebar" = 6 + 3 + 9 = 18
        let two: Vec<(&str, &str)> = vec![("q", "quit"), ("b", "sidebar")];
        assert_eq!(hints_width_with_separators(&two), 18);
    }

    #[test]
    fn test_hints_width_spaced() {
        assert_eq!(hints_width_spaced(&[]), 0);
        let one: Vec<(&str, &str)> = vec![("q", "quit")];
        assert_eq!(hints_width_spaced(&one), 6);
        // "q quit" + " " + "b sidebar" = 6 + 1 + 9 = 16
        let two: Vec<(&str, &str)> = vec![("q", "quit"), ("b", "sidebar")];
        assert_eq!(hints_width_spaced(&two), 16);
    }

    #[test]
    fn test_server_focus_uses_sidebar_hints() {
        let theme = Theme::new();
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, FocusRegion::Servers);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("toggle"),
            "servers focus should show sidebar hints: {content}"
        );
    }
}
