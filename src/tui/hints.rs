// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Keybinds panel for the TUI.
//!
//! Renders keybind groups vertically (one per line) inside a collapsible
//! panel on the left sidebar. Replaces the previous bottom hints bar.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

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

/// Number of content lines when the keybinds panel is expanded.
#[allow(
    clippy::cast_possible_truncation,
    reason = "bind array has single-digit length"
)]
pub const KEYBINDS_EXPANDED_HEIGHT: u16 = KEYBINDS.len() as u16;

/// Render keybinds vertically into the given area.
///
/// Flat list — one keybind per line: `  key  label`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_keybinds_content(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if area.width < 4 || area.height == 0 {
        return;
    }

    let max_rows = area.height;
    for (row, (key, label)) in KEYBINDS.iter().enumerate() {
        if row as u16 >= max_rows {
            break;
        }
        let line = Line::from(vec![
            Span::styled(format!("  {key:<8}"), theme.hint_key),
            Span::styled((*label).to_string(), theme.hint_label),
        ]);
        buf.set_line(area.x, area.y + row as u16, &line, area.width);
    }
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
        // Only 3 rows — should show the first 3 keybinds.
        let backend = TestBackend::new(30, 3);
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
        // Later keybinds (yank) shouldn't fit in 3 rows.
        assert!(
            !content.contains("yank"),
            "should not show yank in 3 rows: {content}"
        );
    }
}
