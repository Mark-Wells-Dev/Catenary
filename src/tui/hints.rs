// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Navigation hints bar rendered at the bottom of the grid.
//!
//! Uses degradation from [`super::degradation::degrade_hints`] to determine
//! which hints fit, and renders them between styled border caps.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;

/// All navigation hints in display order.
const ALL_HINTS: [(&str, &str); 5] = [
    ("z", "\u{2550}"), // ═
    ("v", "\u{2592}"), // ▒
    ("f", "\u{2591}"), // ░
    ("?", ""),
    ("q", "\u{2718}"), // ✘
];

/// Return the navigation hints that fit in the given width.
///
/// Degradation order:
/// 1. All 5 hints with `╱` separators.
/// 2. All 5 hints, space-separated.
/// 3. Drop `z ═`.
/// 4. Drop `v ▒`.
/// 5. Drop `f ░`.
/// 6. Drop `?`.
/// 7. Drop `q ✘`.
/// 8. Empty (border only).
#[must_use]
fn degrade_hints(max_width: u16) -> Vec<(&'static str, &'static str)> {
    let max = max_width as usize;
    let all = ALL_HINTS.to_vec();

    // Level 1: all hints with separators.
    if hints_width_with_separators(&all) <= max {
        return all;
    }

    // Level 2: all hints, space-separated.
    if hints_width_spaced(&all) <= max {
        return all;
    }

    // Levels 3–7: progressively drop hints from the front.
    for drop_count in 1..=4 {
        let remaining = ALL_HINTS[drop_count..].to_vec();
        if hints_width_spaced(&remaining) <= max {
            return remaining;
        }
    }

    // Level 8: empty.
    Vec::new()
}

/// Render navigation hints into a 1-row area at the bottom of the grid.
///
/// Uses [`degrade_hints`] to select which hints fit. Renders hints between
/// border caps: `──┤ hints ├──┘` (light) or `━━┥ hints ┝━━┛` (heavy when
/// `focused_on_bottom` is true).
///
/// When `filter_active` is true, no hints are rendered (the filter bar
/// occupies this space). When `filter_locked` is true, the quit hint `q ✘`
/// is replaced with `Esc ▓` to indicate the locked filter can be cleared.
/// When `debug_active` is true, `[debug]` is shown at the right end.
#[allow(
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools,
    reason = "terminal coordinates are always small; render flags are independent display toggles"
)]
pub fn render_hints(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    filter_active: bool,
    filter_locked: bool,
    focused_on_bottom: bool,
    debug_active: bool,
) {
    if area.width < 4 || area.height < 1 || filter_active {
        return;
    }

    let hint_budget = area.width.saturating_sub(6);
    let mut hints = degrade_hints(hint_budget);

    if hints.is_empty() {
        // Just render the border line.
        render_border_only(area, buf, theme, focused_on_bottom);
        return;
    }

    // Replace "q ✘" with "Esc ▓" when filter is locked.
    if filter_locked {
        for hint in &mut hints {
            if hint.0 == "q" && hint.1 == "\u{2718}" {
                *hint = ("Esc", "\u{2593}");
            }
        }
    }

    // Build hint spans.
    let total_width_with_seps = hints_width_with_separators(&hints);
    let total_width_spaced = hints_width_spaced(&hints);
    let use_separators = total_width_with_seps <= hint_budget as usize;

    let mut hint_spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, symbol)) in hints.iter().enumerate() {
        if i > 0 {
            if use_separators {
                hint_spans.push(Span::styled(" \u{2571} ", theme.muted)); // ╱
            } else {
                hint_spans.push(Span::raw(" "));
            }
        }
        hint_spans.push(Span::styled((*key).to_string(), theme.hint_key));
        if !symbol.is_empty() {
            hint_spans.push(Span::raw(" "));
            hint_spans.push(Span::styled((*symbol).to_string(), theme.hint_label));
        }
    }

    let hints_text_width = if use_separators {
        total_width_with_seps
    } else {
        total_width_spaced
    };

    // Border characters.
    let (h_line, left_cap, right_cap, corner) = if focused_on_bottom {
        ("\u{2501}", "\u{2521}", "\u{251D}", "\u{251B}") // ━ ┡ ┝ ┛
    } else {
        ("\u{2500}", "\u{2524}", "\u{251C}", "\u{2518}") // ─ ┤ ├ ┘
    };

    // Debug indicator: " [debug]" = 8 columns.
    let debug_label = " [debug]";
    let debug_width = if debug_active {
        UnicodeWidthStr::width(debug_label)
    } else {
        0
    };

    // Compute left fill and right fill.
    // Fill pattern: left_fill, left_cap, space, hints, space, right_cap, right_fill, debug?, corner
    let inner_used = 1 + 1 + hints_text_width + 1 + 1 + debug_width + 1; // left_cap, space, hints, space, right_cap, debug?, corner
    let fill_total = (area.width as usize).saturating_sub(inner_used);
    let fill_right = fill_total / 2;
    let fill_left = fill_total.saturating_sub(fill_right);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Left fill.
    if fill_left > 0 {
        spans.push(Span::styled(
            h_line.repeat(fill_left),
            theme.border_unfocused,
        ));
    }

    // Left cap.
    spans.push(Span::styled(left_cap.to_string(), theme.border_unfocused));
    spans.push(Span::raw(" "));

    // Hints content.
    spans.extend(hint_spans);

    // Right cap.
    spans.push(Span::raw(" "));
    spans.push(Span::styled(right_cap.to_string(), theme.border_unfocused));

    // Right fill.
    if fill_right > 0 {
        spans.push(Span::styled(
            h_line.repeat(fill_right),
            theme.border_unfocused,
        ));
    }

    // Debug indicator.
    if debug_active {
        spans.push(Span::styled(debug_label.to_string(), theme.muted));
    }

    // Corner.
    spans.push(Span::styled(corner.to_string(), theme.border_unfocused));

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}

/// Render just the border line when no hints fit.
fn render_border_only(area: Rect, buf: &mut Buffer, theme: &Theme, focused_on_bottom: bool) {
    let (h_line, corner) = if focused_on_bottom {
        ("\u{2501}", "\u{251B}") // ━ ┛
    } else {
        ("\u{2500}", "\u{2518}") // ─ ┘
    };

    let fill = area.width.saturating_sub(1) as usize;
    let mut spans: Vec<Span<'static>> = Vec::new();
    if fill > 0 {
        spans.push(Span::styled(h_line.repeat(fill), theme.border_unfocused));
    }
    spans.push(Span::styled(corner.to_string(), theme.border_unfocused));

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}

/// Display width of a single hint: `key symbol` or just `key`.
fn hint_display_width(key: &str, symbol: &str) -> usize {
    if symbol.is_empty() {
        UnicodeWidthStr::width(key)
    } else {
        UnicodeWidthStr::width(key) + 1 + UnicodeWidthStr::width(symbol)
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
    fn test_hints_render_full() {
        let theme = Theme::new();
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, false, false, false, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should contain hint keys and separators.
        assert!(content.contains('q'), "expected 'q' hint key in: {content}");
        assert!(content.contains('?'), "expected '?' hint key in: {content}");
        assert!(
            content.contains('\u{2571}'),
            "expected separator ╱ at this width: {content}"
        );
        // Border fill should be present at left edge.
        assert!(
            content.starts_with('\u{2500}'),
            "left edge should be border line ─: {content}"
        );
        // Corner at right edge.
        assert!(
            content.trim_end().ends_with('\u{2518}'),
            "right edge should be corner ┘: {content}"
        );
    }

    #[test]
    fn test_hints_render_narrow_border_only() {
        let theme = Theme::new();
        // Width 5: hint_budget = 0, degrade_hints returns empty → border only.
        let backend = TestBackend::new(5, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, false, false, false, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should have border line and corner, no hint keys.
        assert!(
            !content.contains('q'),
            "no hints at narrow width: {content}"
        );
        assert!(
            content.contains('\u{2500}'),
            "should have border line ─: {content}"
        );
        assert!(
            content.trim_end().ends_with('\u{2518}'),
            "should end with corner ┘: {content}"
        );
    }

    #[test]
    fn test_hints_render_too_narrow() {
        let theme = Theme::new();
        // Width 3: below minimum 4, renders nothing.
        let backend = TestBackend::new(3, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, false, false, false, false);
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
    fn test_hints_render_filter_mode() {
        let theme = Theme::new();
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, true, false, false, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // With filter_active=true, no hints should be rendered.
        assert!(
            !content.contains('q'),
            "no hints when filter active: {content}"
        );
        assert!(
            !content.contains('?'),
            "no hints when filter active: {content}"
        );
    }

    #[test]
    fn test_hints_render_locked_filter() {
        let theme = Theme::new();
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, false, true, false, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // With filter_locked=true, "q ✘" should be replaced with "Esc ▓".
        assert!(
            content.contains("Esc"),
            "expected 'Esc' for locked filter in: {content}"
        );
        assert!(
            content.contains('\u{2593}'),
            "expected '▓' for locked filter in: {content}"
        );
    }

    #[test]
    fn test_hints_heavy_caps_when_focused() {
        let theme = Theme::new();
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, false, false, true, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Heavy caps: ┡ and ┝.
        assert!(
            content.contains('\u{2521}'),
            "expected heavy left cap '\u{2521}' in: {content}"
        );
        assert!(
            content.contains('\u{251D}'),
            "expected heavy right cap '\u{251D}' in: {content}"
        );
    }

    #[test]
    fn test_hints_debug_indicator() {
        let theme = Theme::new();
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, false, false, false, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("[debug]"),
            "expected '[debug]' indicator in: {content}"
        );
    }

    #[test]
    fn test_hints_no_debug_indicator_by_default() {
        let theme = Theme::new();
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_hints(area, f.buffer_mut(), &theme, false, false, false, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            !content.contains("[debug]"),
            "no '[debug]' indicator by default: {content}"
        );
    }

    // ── Width calculation tests ─────────────────────────────────────────

    #[test]
    fn test_hints_width_with_separators() {
        // Empty.
        assert_eq!(hints_width_with_separators(&[]), 0);
        // Single hint: no separators, just content.
        let one = vec![("q", "\u{2718}")]; // "q ✘" = 3 cols
        assert_eq!(hints_width_with_separators(&one), 3);
        // Two hints: content + 1 separator (3 cols: " ╱ ").
        let two = vec![("q", "\u{2718}"), ("?", "")]; // 3 + 1 + 3 = 7
        assert_eq!(hints_width_with_separators(&two), 7);
        // Three hints.
        let three = vec![("q", "\u{2718}"), ("?", ""), ("f", "\u{2591}")]; // 3+1+3 + 3+3 = 13
        assert_eq!(hints_width_with_separators(&three), 13);
    }

    #[test]
    fn test_hints_width_spaced() {
        assert_eq!(hints_width_spaced(&[]), 0);
        let one = vec![("q", "\u{2718}")]; // 3 cols, no spaces
        assert_eq!(hints_width_spaced(&one), 3);
        let two = vec![("q", "\u{2718}"), ("?", "")]; // 3 + 1 + 1 = 5
        assert_eq!(hints_width_spaced(&two), 5);
        let three = vec![("q", "\u{2718}"), ("?", ""), ("f", "\u{2591}")]; // 3+1+3 + 1+1 = 9
        assert_eq!(hints_width_spaced(&three), 9);
    }
}
