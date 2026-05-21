// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Unified message stream with hex badges and scrolling.
//!
//! The stream is the primary view surface for TUI v2: a full-width,
//! scrollable, chronological list of messages from the database,
//! prefixed with per-session hex badges.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::format::format_message_styled;
use super::icons::IconSet;
use super::scrollbar::{self, ScrollMetrics};
use super::theme::Theme;
use crate::session::SessionMessage;

// ── Hex badge assignment ──────────────────────────────────────────────

/// Maps session IDs to sequential hex badges (`00`–`FF`).
///
/// Badges are assigned from the lowest available slot. When a session is
/// released via [`release`](Self::release), its badge returns to the free
/// pool and can be reused by a future session. This prevents exhaustion
/// when a daemon runs for weeks with sessions connecting and disconnecting.
pub struct HexBadgeMap {
    map: HashMap<String, u8>,
    /// Sorted pool of badges returned by [`release`](Self::release).
    free: Vec<u8>,
    next: u8,
}

impl HexBadgeMap {
    /// Create an empty badge map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            free: Vec::new(),
            next: 0,
        }
    }

    /// Get or assign a hex badge for a session ID.
    ///
    /// Returns a two-character uppercase hex string (e.g., `"00"`, `"1A"`).
    /// Reuses the lowest available badge from the free pool before
    /// allocating a new one.
    pub fn badge(&mut self, session_id: &str) -> String {
        let id = *self.map.entry(session_id.to_string()).or_insert_with(|| {
            if let Some(recycled) = self.free.pop() {
                recycled
            } else {
                let id = self.next;
                self.next = self.next.wrapping_add(1);
                id
            }
        });
        format!("{id:02X}")
    }

    /// Look up an already-assigned badge without allocating.
    ///
    /// Returns `"??"` if the session has no badge (should not happen in
    /// normal operation — badges are assigned in [`StreamState::new`] and
    /// [`StreamState::append`]).
    #[must_use]
    pub fn get(&self, session_id: &str) -> String {
        self.map
            .get(session_id)
            .map_or_else(|| "??".to_string(), |id| format!("{id:02X}"))
    }

    /// Release a session's badge back to the free pool.
    ///
    /// The badge becomes available for the next new session. No-op if the
    /// session ID has no assigned badge.
    pub fn release(&mut self, session_id: &str) {
        if let Some(id) = self.map.remove(session_id) {
            // Insert sorted so the lowest badge is always at the end
            // (popped first by `badge()`).
            let pos = self.free.partition_point(|&x| x > id);
            self.free.insert(pos, id);
        }
    }
}

impl Default for HexBadgeMap {
    fn default() -> Self {
        Self::new()
    }
}

// ── Stream state ──────────────────────────────────────────────────────

/// Scroll and viewport state for the message stream.
pub struct StreamState {
    /// All messages in chronological order.
    pub messages: Vec<SessionMessage>,
    /// Index of the first visible message in the viewport.
    pub scroll_position: usize,
    /// Whether auto-scroll is active (viewport pinned to bottom).
    pub auto_scroll: bool,
    /// Hex badge assignment for session IDs.
    pub badges: HexBadgeMap,
}

impl StreamState {
    /// Create a new stream state with the given messages.
    #[must_use]
    pub fn new(messages: Vec<SessionMessage>) -> Self {
        let mut badges = HexBadgeMap::new();
        for msg in &messages {
            badges.badge(&msg.session_id);
        }
        Self {
            messages,
            scroll_position: 0,
            auto_scroll: true,
            badges,
        }
    }

    /// Append new messages from a tail reader.
    pub fn append(&mut self, messages: Vec<SessionMessage>) {
        for msg in &messages {
            self.badges.badge(&msg.session_id);
        }
        self.messages.extend(messages);
    }

    /// Scroll up by `n` lines.
    pub const fn scroll_up(&mut self, n: usize) {
        self.scroll_position = self.scroll_position.saturating_sub(n);
        self.auto_scroll = false;
    }

    /// Scroll down by `n` lines, clamped to content length.
    pub fn scroll_down(&mut self, n: usize, viewport_height: usize) {
        let max = self.messages.len().saturating_sub(viewport_height);
        self.scroll_position = (self.scroll_position + n).min(max);
        // Re-enable auto-scroll if we've reached the bottom.
        if self.scroll_position >= max {
            self.auto_scroll = true;
        }
    }

    /// Pin scroll to the bottom of the stream.
    pub const fn pin_to_bottom(&mut self, viewport_height: usize) {
        self.scroll_position = self.messages.len().saturating_sub(viewport_height);
        self.auto_scroll = true;
    }

    /// Update scroll position if auto-scroll is active.
    ///
    /// Call this before rendering so the draw function is read-only.
    pub const fn apply_auto_scroll(&mut self, viewport_height: usize) {
        if self.auto_scroll {
            self.scroll_position = self.messages.len().saturating_sub(viewport_height);
        }
    }

    /// Return [`ScrollMetrics`] for the scrollbar.
    #[must_use]
    pub const fn scroll_metrics(&self, viewport_height: usize) -> ScrollMetrics {
        ScrollMetrics {
            content_length: self.messages.len(),
            viewport_length: viewport_height,
            position: self.scroll_position,
        }
    }
}

// ── Rendering ─────────────────────────────────────────────────────────

/// Render the message stream into the given area.
///
/// Each message line: `HH:MM:SS XX <formatted content>` where `XX` is
/// the hex badge. The scrollbar occupies the rightmost column.
///
/// Call [`StreamState::apply_auto_scroll`] before rendering to update
/// the scroll position — this function is read-only.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_stream(
    state: &StreamState,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    icons: &IconSet,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }

    let viewport_height = area.height as usize;

    // Reserve rightmost column for scrollbar.
    let content_width = area.width.saturating_sub(1);
    let scrollbar_area = Rect {
        x: area.x + content_width,
        y: area.y,
        width: 1,
        height: area.height,
    };

    // Render visible messages.
    for row in 0..viewport_height {
        let msg_idx = state.scroll_position + row;
        if msg_idx >= state.messages.len() {
            break;
        }

        let msg = &state.messages[msg_idx];
        let badge = state.badges.get(&msg.session_id);

        // Build line: badge prefix + formatted message content.
        let styled = format_message_styled(msg, icons, theme);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(styled.spans.len() + 1);
        spans.push(Span::styled(format!("{badge} "), theme.accent));
        spans.extend(styled.spans);

        let line = Line::from(spans);
        let y = area.y + row as u16;
        buf.set_line(area.x, y, &line, content_width);
    }

    // Render scrollbar.
    let metrics = state.scroll_metrics(viewport_height);
    scrollbar::render_scrollbar(
        &metrics,
        scrollbar_area,
        buf,
        ratatui::style::Color::DarkGray,
        ratatui::style::Color::Black,
    );

    // Render overflow counts.
    let counts = scrollbar::compute_overflow(&metrics);
    let content_area = Rect {
        x: area.x,
        y: area.y,
        width: content_width,
        height: area.height,
    };
    scrollbar::render_overflow_counts(&counts, content_area, buf, theme.muted);
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::session::test_support;

    fn make_message(session_id: &str, method: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            ..test_support::message("lsp", method, "rust-analyzer")
        }
    }

    // ── Hex badge tests ───────────────────────────────────────────────

    #[test]
    fn test_hex_badge_sequential_assignment() {
        let mut badges = HexBadgeMap::new();
        assert_eq!(badges.badge("session-a"), "00");
        assert_eq!(badges.badge("session-b"), "01");
        assert_eq!(badges.badge("session-a"), "00"); // stable
        assert_eq!(badges.badge("session-c"), "02");
    }

    #[test]
    fn test_hex_badge_format_uppercase() {
        let mut badges = HexBadgeMap::new();
        // Fill up to 0x0A.
        for i in 0..10 {
            badges.badge(&format!("s{i}"));
        }
        assert_eq!(badges.badge("s-ten"), "0A");
    }

    #[test]
    fn test_hex_badge_release_and_reuse() {
        let mut badges = HexBadgeMap::new();
        assert_eq!(badges.badge("s0"), "00");
        assert_eq!(badges.badge("s1"), "01");
        assert_eq!(badges.badge("s2"), "02");

        // Release the middle badge.
        badges.release("s1");

        // Next new session gets the recycled "01", not "03".
        assert_eq!(badges.badge("s3"), "01");

        // Existing sessions are unaffected.
        assert_eq!(badges.badge("s0"), "00");
        assert_eq!(badges.badge("s2"), "02");

        // Next truly new session gets "03".
        assert_eq!(badges.badge("s4"), "03");
    }

    #[test]
    fn test_hex_badge_release_lowest_first() {
        let mut badges = HexBadgeMap::new();
        badges.badge("s0"); // 00
        badges.badge("s1"); // 01
        badges.badge("s2"); // 02

        // Release out of order.
        badges.release("s2");
        badges.release("s0");

        // Should reclaim "00" first (lowest), then "02".
        assert_eq!(badges.badge("s-new-1"), "00");
        assert_eq!(badges.badge("s-new-2"), "02");
    }

    #[test]
    fn test_hex_badge_release_unknown_is_noop() {
        let mut badges = HexBadgeMap::new();
        badges.badge("s0");
        badges.release("nonexistent"); // no-op
        assert_eq!(badges.badge("s1"), "01"); // unaffected
    }

    // ── Stream state tests ────────────────────────────────────────────

    #[test]
    fn test_stream_state_auto_scroll() {
        let messages: Vec<_> = (0..20)
            .map(|i| make_message("s1", &format!("method-{i}")))
            .collect();
        let state = StreamState::new(messages);

        assert!(state.auto_scroll);
        // Auto-scroll pins to bottom.
        assert_eq!(state.scroll_position, 0); // not yet pinned — render does it
    }

    #[test]
    fn test_stream_state_scroll_up_disables_auto() {
        let messages: Vec<_> = (0..20)
            .map(|i| make_message("s1", &format!("method-{i}")))
            .collect();
        let mut state = StreamState::new(messages);
        state.pin_to_bottom(10);

        state.scroll_up(3);
        assert!(!state.auto_scroll);
        assert_eq!(state.scroll_position, 7); // 10 - 3
    }

    #[test]
    fn test_stream_state_scroll_down_re_enables_auto() {
        let messages: Vec<_> = (0..20)
            .map(|i| make_message("s1", &format!("method-{i}")))
            .collect();
        let mut state = StreamState::new(messages);
        state.pin_to_bottom(10);
        state.scroll_up(5);

        state.scroll_down(5, 10);
        assert!(state.auto_scroll);
    }

    #[test]
    fn test_stream_state_append_assigns_badges() {
        let mut state = StreamState::new(vec![make_message("s1", "hover")]);
        assert_eq!(state.badges.badge("s1"), "00");

        state.append(vec![make_message("s2", "definition")]);
        assert_eq!(state.badges.badge("s2"), "01");
    }

    #[test]
    fn test_stream_scroll_metrics() {
        let messages: Vec<_> = (0..30)
            .map(|i| make_message("s1", &format!("m-{i}")))
            .collect();
        let mut state = StreamState::new(messages);
        state.pin_to_bottom(10);

        let metrics = state.scroll_metrics(10);
        assert_eq!(metrics.content_length, 30);
        assert_eq!(metrics.viewport_length, 10);
        assert_eq!(metrics.position, 20);
    }

    // ── Render tests ──────────────────────────────────────────────────

    #[test]
    fn test_render_stream_shows_hex_badges() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let messages = vec![
            make_message("session-a", "textDocument/hover"),
            make_message("session-b", "textDocument/definition"),
        ];
        let state = StreamState::new(messages);

        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());

        let backend = TestBackend::new(60, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_stream(&state, area, frame.buffer_mut(), &theme, &icons);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("00"),
            "expected hex badge '00' in output, got: {content}"
        );
        assert!(
            content.contains("01"),
            "expected hex badge '01' in output, got: {content}"
        );
    }

    #[test]
    fn test_render_stream_narrow_guard() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = StreamState::new(vec![make_message("s1", "hover")]);
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());

        // Width 3 < 4: guard returns early.
        let backend = TestBackend::new(3, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_stream(&state, area, frame.buffer_mut(), &theme, &icons);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);
        let non_space = content.replace([' ', '\n'], "");
        assert!(
            non_space.is_empty(),
            "narrow terminal should produce empty output, got: {content}"
        );
    }

    /// Convert a ratatui buffer to a string for assertion matching.
    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
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
