// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Keybinds panel for the TUI.
//!
//! Renders keybind groups vertically (one per line) inside a collapsible
//! panel on the left sidebar. Replaces the previous bottom hints bar.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::format::truncate_to_width;
use super::theme::Theme;

/// All keybindings (flat list, no modal sections). This popup is the sole full
/// key list now that the footer slimmed to `? keys` (tui-rework 11, item 2), so
/// every binding a hint ever carried — including `a` (apply fix-it) — lives
/// here.
const KEYBINDS: &[(&str, &str)] = &[
    ("j/k", "navigate"),
    ("Tab", "cycle panes"),
    ("Enter", "expand / focus"),
    ("a", "apply fix-it"),
    ("p", "problems only"),
    ("d", "dormant toggle"),
    ("y", "yank scope id"),
    ("PgUp/Dn", "scroll"),
    ("Home/End", "jump"),
    ("?", "toggle keybinds"),
    ("q", "quit"),
];

/// Minimum gap between the key column and its description (tui-rework 12, item
/// 2): without it the widest key runs into its label — `Home/Endjump`.
const KEY_GUTTER: usize = 2;

/// Total popup height: one row per keybind plus the top and bottom border rows
/// (tui-rework 12, item 2 — the popup is now bordered, not borderless).
#[allow(
    clippy::cast_possible_truncation,
    reason = "bind array has single-digit length"
)]
pub const KEYBINDS_EXPANDED_HEIGHT: u16 = KEYBINDS.len() as u16 + 2;

/// Render the bordered keybinds popup into the given area.
///
/// A flat list — one keybind per line, `key<gutter>label` — inside a border that
/// matches the grid's frame (tui-rework 12, item 2). The key column is padded to
/// the widest key plus [`KEY_GUTTER`] spaces so no key touches its description.
/// Every content line is width-bounded so the popup never overflows a narrow
/// terminal.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_keybinds_content(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    draw_border(area, buf, theme.border_unfocused);

    // Key column: the widest key plus the gutter, so descriptions align and none
    // is touched by its key.
    let key_col = KEYBINDS
        .iter()
        .map(|(k, _)| UnicodeWidthStr::width(*k))
        .max()
        .unwrap_or(0)
        + KEY_GUTTER;

    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2) as usize;
    let max_rows = area.height.saturating_sub(2);
    for (row, (key, label)) in KEYBINDS.iter().enumerate() {
        if row as u16 >= max_rows {
            break;
        }
        let key_cell = format!("{key:<key_col$}");
        let line = Line::from(vec![
            Span::styled(truncate_to_width(&key_cell, inner_w), theme.hint_key),
            Span::styled(
                truncate_to_width(label, inner_w.saturating_sub(key_col)),
                theme.hint_label,
            ),
        ]);
        buf.set_line(inner_x, area.y + 1 + row as u16, &line, area.width - 2);
    }
}

/// Draw a light box-drawing border around `area`, matching the grid's frame.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
fn draw_border(area: Rect, buf: &mut Buffer, style: ratatui::style::Style) {
    let x0 = area.x;
    let y0 = area.y;
    let x1 = area.x + area.width - 1;
    let y1 = area.y + area.height - 1;
    let mut put = |x: u16, y: u16, s: &str| {
        if x <= x1 && y <= y1 && x < buf.area.right() && y < buf.area.bottom() {
            buf.set_string(x, y, s, style);
        }
    };
    for x in x0..=x1 {
        put(x, y0, "─");
        put(x, y1, "─");
    }
    for y in y0..=y1 {
        put(x0, y, "│");
        put(x1, y, "│");
    }
    put(x0, y0, "┌");
    put(x1, y0, "┐");
    put(x0, y1, "└");
    put(x1, y1, "┘");
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
    fn keybinds_content_shows_flat_list() {
        let theme = Theme::new();
        let backend = TestBackend::new(30, 20);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_keybinds_content(area, f.buffer_mut(), &theme);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("quit"), "should show quit: {content}");
        assert!(
            content.contains("cycle panes"),
            "should show Tab hint: {content}"
        );
        assert!(
            content.contains("toggle keybinds"),
            "should show ? hint: {content}"
        );
        assert!(
            content.contains("navigate"),
            "should show navigate: {content}"
        );
        assert!(
            content.contains("problems only"),
            "should show problems-only: {content}"
        );
        assert!(content.contains("yank"), "should show yank: {content}");
        assert!(content.contains("scroll"), "should show scroll: {content}");
        assert!(content.contains("jump"), "should show jump: {content}");
        // No section headers in the flat layout.
        assert!(
            !content.contains("Sidebar"),
            "should not show Sidebar header: {content}"
        );
        assert!(
            !content.contains("Stream"),
            "should not show Stream header: {content}"
        );
    }

    #[test]
    fn keybinds_content_narrow_guard() {
        let theme = Theme::new();
        let area = Rect::new(0, 0, 3, 5);
        let mut buf = Buffer::empty(area);
        render_keybinds_content(area, &mut buf, &theme);

        let content = buffer_to_string(&buf);
        let non_space = content.replace([' ', '\n'], "");
        assert!(
            non_space.is_empty(),
            "width < 4 should produce empty output, got: {content}"
        );
    }

    #[test]
    fn keybinds_content_truncates_at_height() {
        let theme = Theme::new();
        // 5 rows total → 2 border rows + 3 content rows: the first 3 keybinds.
        let backend = TestBackend::new(30, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_keybinds_content(area, f.buffer_mut(), &theme);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("navigate"),
            "should show first keybind: {content}"
        );
        assert!(
            content.contains("cycle panes"),
            "should show second keybind: {content}"
        );
        // Later keybinds (yank) shouldn't fit in the 3 content rows.
        assert!(
            !content.contains("yank"),
            "should not show yank in 3 content rows: {content}"
        );
    }

    #[test]
    fn keybinds_popup_has_gutter_and_border() {
        // tui-rework 12, item 2: the widest key (`Home/End`, 8 cols) must not
        // touch its description, and the popup is bordered on all four sides.
        let theme = Theme::new();
        let backend = TestBackend::new(30, 15);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_keybinds_content(area, f.buffer_mut(), &theme);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // The regression: `Home/End` ran straight into `jump`. With the gutter
        // there is whitespace between key and description.
        assert!(
            content.contains("Home/End  jump") || content.contains("Home/End "),
            "widest key keeps a gutter before its description: {content}"
        );
        assert!(
            !content.contains("Home/Endjump"),
            "key must not touch its description: {content}"
        );
        // A border on all four sides: corners present.
        for corner in ['┌', '┐', '└', '┘'] {
            assert!(
                content.contains(corner),
                "popup has a {corner} corner: {content}"
            );
        }
    }

    #[test]
    fn keybinds_popup_is_width_bounded_when_narrow() {
        // Every rendered line stays within the popup width on a narrow terminal
        // (tui-rework 11 item 1 still holds — nothing overflows).
        let theme = Theme::new();
        let area = Rect::new(0, 0, 12, 15);
        let mut buf = Buffer::empty(area);
        render_keybinds_content(area, &mut buf, &theme);
        let content = buffer_to_string(&buf);
        for line in content.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 12,
                "no popup line exceeds its width: {line:?}"
            );
        }
    }
}
