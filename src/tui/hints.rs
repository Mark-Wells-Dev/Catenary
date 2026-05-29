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

/// Global keybindings (always active).
const GLOBAL_BINDS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("Tab", "next panel"),
    ("b", "cycle tabs"),
    ("?", "toggle keybinds"),
];

/// Sidebar keybindings (sessions/servers panels).
const SIDEBAR_BINDS: &[(&str, &str)] = &[
    ("j/k", "navigate"),
    ("h/l", "scroll"),
    ("Space", "toggle filter"),
    ("Enter", "expand/collapse"),
];

/// Stream keybindings.
const STREAM_BINDS: &[(&str, &str)] = &[
    ("j/k", "navigate"),
    ("Enter", "expand/collapse"),
    ("y", "yank"),
    ("/", "search"),
    ("n/N", "next/prev match"),
    ("PgUp/Dn", "scroll"),
    ("Home/End", "jump"),
];

/// Number of content lines when the keybinds panel is expanded.
///
/// Computed from the three bind groups plus group headers and blank separators.
#[allow(
    clippy::cast_possible_truncation,
    reason = "bind arrays have single-digit lengths"
)]
pub const KEYBINDS_EXPANDED_HEIGHT: u16 = GLOBAL_BINDS.len() as u16
    + 1 // blank
    + 1 // "Sidebar" header
    + SIDEBAR_BINDS.len() as u16
    + 1 // blank
    + 1 // "Stream" header
    + STREAM_BINDS.len() as u16;

/// Render keybinds vertically into the given area.
///
/// Groups: Global, Sidebar, Stream. Each keybind is one line:
/// `  key  label`. Group headers are bold.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_keybinds_content(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if area.width < 4 || area.height == 0 {
        return;
    }

    let mut row: u16 = 0;
    let max_rows = area.height;

    let render_group = |binds: &[(&str, &str)], row: &mut u16, buf: &mut Buffer, theme: &Theme| {
        for (key, label) in binds {
            if *row >= max_rows {
                return;
            }
            let line = Line::from(vec![
                Span::styled(format!("  {key:<8}"), theme.hint_key),
                Span::styled((*label).to_string(), theme.hint_label),
            ]);
            buf.set_line(area.x, area.y + *row, &line, area.width);
            *row += 1;
        }
    };

    // Global.
    render_group(GLOBAL_BINDS, &mut row, buf, theme);

    // Blank line.
    if row < max_rows {
        row += 1;
    }

    // Sidebar header.
    if row < max_rows {
        let header = Line::from(Span::styled("Sidebar", theme.title));
        buf.set_line(area.x, area.y + row, &header, area.width);
        row += 1;
    }
    render_group(SIDEBAR_BINDS, &mut row, buf, theme);

    // Blank line.
    if row < max_rows {
        row += 1;
    }

    // Stream header.
    if row < max_rows {
        let header = Line::from(Span::styled("Stream", theme.title));
        buf.set_line(area.x, area.y + row, &header, area.width);
        row += 1;
    }
    render_group(STREAM_BINDS, &mut row, buf, theme);
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
    fn keybinds_content_shows_all_groups() {
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
            content.contains("next panel"),
            "should show Tab hint: {content}"
        );
        assert!(
            content.contains("toggle keybinds"),
            "should show ? hint: {content}"
        );
        assert!(
            content.contains("Sidebar"),
            "should show Sidebar group: {content}"
        );
        assert!(
            content.contains("navigate"),
            "should show navigate: {content}"
        );
        assert!(
            content.contains("Stream"),
            "should show Stream group: {content}"
        );
        assert!(content.contains("yank"), "should show yank: {content}");
        assert!(content.contains("expand"), "should show expand: {content}");
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
        // Only 3 rows — should show global binds only.
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

        assert!(content.contains("quit"), "should show quit: {content}");
        // Sidebar header shouldn't fit in 3 rows (3 global binds fill it).
        assert!(
            !content.contains("Sidebar"),
            "should not show Sidebar in 3 rows: {content}"
        );
    }
}
