// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server message detail panel.
//!
//! Shows a scrollable history of `window/logMessage` and `window/showMessage`
//! entries for a single server instance. Rendered above the stream panel on
//! the right side. Opened with Enter from the Servers panel, closed with
//! Esc or q.

use chrono::Local;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};

use super::data::ServerMessageDetail;
use super::theme::Theme;

/// State for the server message detail panel.
pub struct ServerPopup {
    /// Title for the panel (server name + root).
    pub title: String,
    /// Message history, newest first.
    pub messages: Vec<ServerMessageDetail>,
    /// Scroll offset (index of first visible wrapped line).
    pub scroll_offset: usize,
    /// Total wrapped line count from the last render pass.
    ///
    /// Updated by [`render_server_detail`] so scroll clamping accounts for
    /// word-wrapped lines, not just message count.
    pub total_lines: usize,
}

impl ServerPopup {
    /// Create a new detail panel for the given server instance.
    #[must_use]
    pub fn new(server_name: &str, root: &str, messages: Vec<ServerMessageDetail>) -> Self {
        let title = if root.is_empty() {
            format!(" {server_name} ")
        } else {
            format!(" {server_name} ({root}) ")
        };
        Self {
            title,
            messages,
            scroll_offset: 0,
            total_lines: 0,
        }
    }

    /// Scroll up by `n` lines.
    pub const fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Scroll down by `n` lines, clamped to wrapped line count.
    pub fn scroll_down(&mut self, n: usize, visible: usize) {
        let max = self.total_lines.saturating_sub(visible);
        self.scroll_offset = (self.scroll_offset + n).min(max);
    }
}

/// Render the server message detail panel into the given area.
///
/// Draws a bordered frame with the server name as title, then renders
/// message lines with timestamps. Each message gets one or more lines
/// depending on wrapping. Updates `popup.total_lines` for scroll clamping.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_server_detail(
    popup: &mut ServerPopup,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    focused: bool,
    borders: Borders,
) {
    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let title_style = if focused { theme.title } else { theme.muted };
    let block = Block::default()
        .borders(borders)
        .border_style(border_style)
        .title(Span::styled(&popup.title, title_style));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if popup.messages.is_empty() {
        let msg = "No server messages";
        let x = inner.x + inner.width.saturating_sub(msg.len() as u16) / 2;
        let y = inner.y + inner.height / 2;
        buf.set_string(x, y, msg, theme.muted);
        return;
    }

    // Render messages as lines: "HH:MM:SS  message text"
    // Each message may wrap across multiple terminal rows.
    let max_rows = inner.height as usize;
    let content_width = inner.width as usize;
    // Timestamp prefix: "HH:MM:SS  " = 10 chars
    let ts_width = 10;
    let text_width = content_width.saturating_sub(ts_width);

    // Build wrapped lines for all messages, then slice by scroll offset.
    let mut lines: Vec<Line<'_>> = Vec::new();
    for detail in &popup.messages {
        let ts = detail
            .timestamp
            .with_timezone(&Local)
            .format("%H:%M:%S")
            .to_string();

        let msg = &detail.message;
        let method_style = if detail.method == "window/showMessage" {
            theme.warning
        } else {
            theme.muted
        };

        if text_width == 0 {
            lines.push(Line::from(Span::styled(ts, theme.timestamp)));
            continue;
        }

        // Wrap message text into chunks of text_width.
        let chunks: Vec<&str> = wrap_text(msg, text_width);

        for (ci, chunk) in chunks.iter().enumerate() {
            if ci == 0 {
                lines.push(Line::from(vec![
                    Span::styled(format!("{ts}  "), theme.timestamp),
                    Span::styled((*chunk).to_string(), method_style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(ts_width)),
                    Span::styled((*chunk).to_string(), method_style),
                ]));
            }
        }
    }

    // Update total line count for scroll clamping and clamp offset.
    popup.total_lines = lines.len();
    popup.scroll_offset = popup
        .scroll_offset
        .min(popup.total_lines.saturating_sub(max_rows));

    // Apply scroll offset and render visible lines.
    let visible_lines = lines.iter().skip(popup.scroll_offset).take(max_rows);
    for (i, line) in visible_lines.enumerate() {
        let y = inner.y + i as u16;
        buf.set_line(inner.x, y, line, inner.width);
    }
}

/// Split text into chunks that fit within `width` characters.
///
/// Uses char boundaries (not byte offsets) so multi-byte UTF-8 is safe.
/// Breaks on the last space before the width boundary when possible,
/// otherwise breaks at the width limit.
fn wrap_text(text: &str, width: usize) -> Vec<&str> {
    if width == 0 {
        return vec![text];
    }
    let mut result = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let remaining = &text[start..];
        if remaining.chars().count() <= width {
            result.push(remaining);
            break;
        }

        // Find the byte offset of the char at position `width`.
        let boundary = remaining
            .char_indices()
            .nth(width)
            .map_or(remaining.len(), |(byte_idx, _)| byte_idx);

        // Try to break at last space within the boundary.
        let break_at = remaining[..boundary]
            .rfind(' ')
            .map_or(boundary, |pos| pos + 1);

        result.push(&text[start..start + break_at]);
        start += break_at;
    }

    if result.is_empty() {
        result.push("");
    }
    result
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn make_detail(method: &str, message: &str) -> ServerMessageDetail {
        ServerMessageDetail {
            method: method.to_string(),
            message: message.to_string(),
            timestamp: Utc.with_ymd_and_hms(2026, 1, 15, 10, 30, 0).unwrap(),
        }
    }

    #[test]
    fn popup_new_with_root() {
        let popup = ServerPopup::new("rust-analyzer", "Catenary/", vec![]);
        assert_eq!(popup.title, " rust-analyzer (Catenary/) ");
        assert!(popup.messages.is_empty());
        assert_eq!(popup.scroll_offset, 0);
    }

    #[test]
    fn popup_new_empty_root() {
        let popup = ServerPopup::new("rust-analyzer", "", vec![]);
        assert_eq!(popup.title, " rust-analyzer ");
    }

    #[test]
    fn popup_scroll_up_clamps_to_zero() {
        let mut popup = ServerPopup::new("ra", "", vec![]);
        popup.scroll_offset = 2;
        popup.scroll_up(5);
        assert_eq!(popup.scroll_offset, 0);
    }

    #[test]
    fn popup_scroll_down_clamps_to_total_lines() {
        let msgs = vec![make_detail("window/logMessage", "a"); 10];
        let mut popup = ServerPopup::new("ra", "", msgs);
        // Simulate render having computed 10 wrapped lines.
        popup.total_lines = 10;
        popup.scroll_down(100, 5);
        // 10 lines - 5 visible = 5
        assert_eq!(popup.scroll_offset, 5);
    }

    #[test]
    fn popup_scroll_down_uses_wrapped_line_count() {
        // 3 messages, but total_lines reflects wrapping (e.g., 9 lines).
        let msgs = vec![make_detail("window/logMessage", "a"); 3];
        let mut popup = ServerPopup::new("ra", "", msgs);
        popup.total_lines = 9;
        popup.scroll_down(100, 5);
        // 9 wrapped lines - 5 visible = 4
        assert_eq!(popup.scroll_offset, 4);
    }

    #[test]
    fn wrap_text_no_wrap_needed() {
        let result = wrap_text("short", 20);
        assert_eq!(result, vec!["short"]);
    }

    #[test]
    fn wrap_text_breaks_at_space() {
        let result = wrap_text("hello world foo", 12);
        assert_eq!(result, vec!["hello world ", "foo"]);
    }

    #[test]
    fn wrap_text_no_space_breaks_at_width() {
        let result = wrap_text("abcdefghij", 5);
        assert_eq!(result, vec!["abcde", "fghij"]);
    }

    #[test]
    fn wrap_text_empty() {
        let result = wrap_text("", 10);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn wrap_text_multibyte_utf8() {
        // "Index\u{2026} done" — \u{2026} is 3 bytes but 1 char.
        let text = "Index\u{2026} done";
        let result = wrap_text(text, 7);
        assert_eq!(result, vec!["Index\u{2026} ", "done"]);
    }

    #[test]
    fn wrap_text_emoji_no_panic() {
        let text = "\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}";
        let result = wrap_text(text, 3);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].chars().count(), 3);
        assert_eq!(result[1].chars().count(), 2);
    }

    #[test]
    fn render_detail_empty_messages() {
        let theme = Theme::new();
        let mut popup = ServerPopup::new("ra", "proj/", vec![]);
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        render_server_detail(&mut popup, area, &mut buf, &theme, true, Borders::ALL);

        let content = buffer_to_string(&buf);
        assert!(
            content.contains("No server messages"),
            "should show empty state: {content}"
        );
    }

    #[test]
    fn render_detail_shows_messages() {
        let theme = Theme::new();
        let msgs = vec![
            make_detail("window/showMessage", "Failed to load workspaces"),
            make_detail("window/logMessage", "Indexing complete"),
        ];
        let mut popup = ServerPopup::new("rust-analyzer", "Catenary/", msgs);
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_server_detail(&mut popup, area, &mut buf, &theme, true, Borders::ALL);

        let content = buffer_to_string(&buf);
        assert!(
            content.contains("rust-analyzer"),
            "title should show: {content}"
        );
        assert!(
            content.contains("Failed to load workspaces"),
            "should show message: {content}"
        );
        assert!(
            content.contains("Indexing complete"),
            "should show second message: {content}"
        );
    }

    #[test]
    fn render_detail_updates_total_lines() {
        let theme = Theme::new();
        let msgs = vec![
            make_detail("window/logMessage", "msg one"),
            make_detail("window/logMessage", "msg two"),
            make_detail("window/logMessage", "msg three"),
        ];
        let mut popup = ServerPopup::new("ra", "", msgs);
        assert_eq!(popup.total_lines, 0);

        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_server_detail(&mut popup, area, &mut buf, &theme, true, Borders::ALL);

        assert_eq!(popup.total_lines, 3);
    }

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
}
