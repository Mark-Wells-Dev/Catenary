// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Unified message stream with hex badges, pipeline, and scrolling.
//!
//! The stream is the primary view surface for TUI v2: a full-width,
//! scrollable, chronological list of processed display entries from
//! the pipeline, prefixed with per-session hex badges and expandable
//! scope/collapsed groups with tree-drawing characters.

use std::collections::{HashMap, HashSet};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::format::{
    format_collapsed_styled, format_message_styled, format_pair_styled, format_scope_styled,
};
use super::icons::IconSet;
use super::pipeline::{self, DisplayEntry};
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

// ── Display rows ─────────────────────────────────────────────────────

/// A single terminal row in the flattened display list.
///
/// The stream viewport renders display rows, not raw messages or
/// pipeline entries. Expansion state determines which rows exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRow {
    /// A top-level pipeline entry (collapsed by default for runs,
    /// expanded by default for scopes).
    Entry(usize),
    /// A child within an expanded scope, rendered with tree chars.
    ScopeChild {
        /// Index into `StreamState::entries` for the parent scope.
        entry_idx: usize,
        /// Index into the scope's `children` vec.
        child_idx: usize,
        /// Whether this is the last child (for `└─` vs `├─`).
        is_last: bool,
    },
    /// An individual message within an expanded collapsed run.
    ExpandedMessage {
        /// Index into `StreamState::entries` for the collapsed run.
        entry_idx: usize,
        /// Index into `StreamState::messages` for this individual message.
        msg_idx: usize,
        /// Whether this is the last message in the run.
        is_last: bool,
    },
}

// ── Stream state ──────────────────────────────────────────────────────

/// Scroll, cursor, pipeline, and viewport state for the message stream.
pub struct StreamState {
    /// All messages in chronological order.
    pub messages: Vec<SessionMessage>,
    /// Pipeline-processed display entries.
    pub entries: Vec<DisplayEntry>,
    /// Flattened display rows for rendering.
    pub display_rows: Vec<DisplayRow>,
    /// Expansion toggle set. Contains `expansion_index()` values for
    /// entries whose default state has been inverted:
    /// - Scopes default expanded → toggled = collapsed (header only)
    /// - Collapsed runs default collapsed → toggled = expanded (children)
    pub toggled: HashSet<usize>,
    /// Index of the first visible row in the viewport.
    pub scroll_position: usize,
    /// Whether auto-scroll is active (viewport pinned to bottom).
    pub auto_scroll: bool,
    /// Currently focused display row (for Enter expansion toggle).
    pub cursor: usize,
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
        let mut state = Self {
            messages,
            entries: Vec::new(),
            display_rows: Vec::new(),
            toggled: HashSet::new(),
            scroll_position: 0,
            auto_scroll: true,
            cursor: 0,
            badges,
        };
        state.rebuild_pipeline();
        state
    }

    /// Append new messages from a tail reader, then rebuild the pipeline.
    pub fn append(&mut self, messages: Vec<SessionMessage>) {
        for msg in &messages {
            self.badges.badge(&msg.session_id);
        }
        self.messages.extend(messages);
        self.rebuild_pipeline();
    }

    /// Run the display pipeline: pair merge → scope collapse → run collapse.
    pub fn rebuild_pipeline(&mut self) {
        let merged = pipeline::pair_merge(&self.messages);
        let scoped = pipeline::scope_collapse(merged, &self.messages);
        self.entries = pipeline::run_collapse(scoped, &self.messages);
        self.rebuild_display_rows();
    }

    /// Flatten entries into display rows based on expansion state.
    fn rebuild_display_rows(&mut self) {
        self.display_rows.clear();
        for (i, entry) in self.entries.iter().enumerate() {
            let exp_key = entry.expansion_index();
            match entry {
                DisplayEntry::Scope { children, .. } => {
                    self.display_rows.push(DisplayRow::Entry(i));
                    // Scopes default expanded; toggled = collapsed.
                    if !self.toggled.contains(&exp_key) {
                        let len = children.len();
                        for (ci, _) in children.iter().enumerate() {
                            self.display_rows.push(DisplayRow::ScopeChild {
                                entry_idx: i,
                                child_idx: ci,
                                is_last: ci == len - 1,
                            });
                        }
                    }
                }
                DisplayEntry::Collapsed {
                    start_index,
                    end_index,
                    ..
                } => {
                    // Collapsed runs default collapsed; toggled = expanded.
                    if self.toggled.contains(&exp_key) {
                        self.display_rows.push(DisplayRow::Entry(i));
                        let start = *start_index;
                        let end = *end_index;
                        for msg_idx in start..=end {
                            self.display_rows.push(DisplayRow::ExpandedMessage {
                                entry_idx: i,
                                msg_idx,
                                is_last: msg_idx == end,
                            });
                        }
                    } else {
                        self.display_rows.push(DisplayRow::Entry(i));
                    }
                }
                DisplayEntry::Single { .. } | DisplayEntry::Paired { .. } => {
                    self.display_rows.push(DisplayRow::Entry(i));
                }
            }
        }
        // Clamp cursor to valid range.
        let max = self.display_rows.len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
    }

    /// Total number of display rows (for scroll calculations).
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.display_rows.len()
    }

    /// Move cursor up by `n` rows.
    pub const fn cursor_up(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n);
        self.auto_scroll = false;
        // Scroll viewport to keep cursor visible.
        if self.cursor < self.scroll_position {
            self.scroll_position = self.cursor;
        }
    }

    /// Move cursor down by `n` rows.
    pub fn cursor_down(&mut self, n: usize, viewport_height: usize) {
        let max = self.row_count().saturating_sub(1);
        self.cursor = (self.cursor + n).min(max);
        // Re-enable auto-scroll if cursor reached the bottom.
        let content_max = self.row_count().saturating_sub(viewport_height);
        if self.cursor >= self.row_count().saturating_sub(1) {
            self.auto_scroll = true;
        }
        // Scroll viewport to keep cursor visible.
        if self.cursor >= self.scroll_position + viewport_height {
            self.scroll_position = self
                .cursor
                .saturating_sub(viewport_height - 1)
                .min(content_max);
        }
    }

    /// Scroll up by `n` lines (viewport-level scroll, cursor follows).
    pub const fn scroll_up(&mut self, n: usize) {
        self.scroll_position = self.scroll_position.saturating_sub(n);
        self.auto_scroll = false;
        // Keep cursor in viewport — clamp to scroll_position.
        if self.cursor < self.scroll_position {
            self.cursor = self.scroll_position;
        }
    }

    /// Scroll down by `n` lines (viewport-level scroll, cursor follows).
    pub fn scroll_down(&mut self, n: usize, viewport_height: usize) {
        let max = self.row_count().saturating_sub(viewport_height);
        self.scroll_position = (self.scroll_position + n).min(max);
        // Re-enable auto-scroll if we've reached the bottom.
        if self.scroll_position >= max {
            self.auto_scroll = true;
        }
        // Keep cursor in viewport.
        let viewport_end = self.scroll_position + viewport_height;
        if self.cursor < self.scroll_position {
            self.cursor = self.scroll_position;
        } else if self.cursor >= viewport_end {
            self.cursor = viewport_end.saturating_sub(1);
        }
    }

    /// Pin scroll and cursor to the bottom of the stream.
    pub const fn pin_to_bottom(&mut self, viewport_height: usize) {
        self.scroll_position = self.row_count().saturating_sub(viewport_height);
        self.cursor = self.row_count().saturating_sub(1);
        self.auto_scroll = true;
    }

    /// Update scroll position if auto-scroll is active.
    ///
    /// Call this before rendering so the draw function is read-only.
    pub const fn apply_auto_scroll(&mut self, viewport_height: usize) {
        if self.auto_scroll {
            self.scroll_position = self.row_count().saturating_sub(viewport_height);
            self.cursor = self.row_count().saturating_sub(1);
        }
    }

    /// Toggle expansion state on the entry at the cursor position.
    ///
    /// Scopes toggle between showing/hiding children. Collapsed runs
    /// toggle between summary and individual messages. Singles and pairs
    /// are not expandable. Resets expansion on scroll-to-bottom.
    pub fn toggle_expansion(&mut self) {
        let Some(row) = self.display_rows.get(self.cursor) else {
            return;
        };
        let entry_idx = match row {
            DisplayRow::Entry(idx) => *idx,
            DisplayRow::ScopeChild { entry_idx, .. }
            | DisplayRow::ExpandedMessage { entry_idx, .. } => *entry_idx,
        };
        let Some(entry) = self.entries.get(entry_idx) else {
            return;
        };
        // Only Scope and Collapsed entries are expandable.
        if !matches!(
            entry,
            DisplayEntry::Scope { .. } | DisplayEntry::Collapsed { .. }
        ) {
            return;
        }
        let exp_key = entry.expansion_index();
        if self.toggled.contains(&exp_key) {
            self.toggled.remove(&exp_key);
        } else {
            self.toggled.insert(exp_key);
        }
        self.rebuild_display_rows();
    }

    /// Return [`ScrollMetrics`] for the scrollbar.
    #[must_use]
    pub const fn scroll_metrics(&self, viewport_height: usize) -> ScrollMetrics {
        ScrollMetrics {
            content_length: self.row_count(),
            viewport_length: viewport_height,
            position: self.scroll_position,
        }
    }
}

// ── Rendering helpers ────────────────────────────────────────────────

/// Tree-drawing prefix for child rows.
const TREE_MID: &str = "├─ ";
/// Tree-drawing prefix for the last child row.
const TREE_END: &str = "└─ ";
/// Blank indent matching the badge width ("XX ").
const BADGE_INDENT: &str = "   ";

/// Get the session ID for a display entry's primary message.
fn entry_session_id<'a>(entry: &DisplayEntry, messages: &'a [SessionMessage]) -> &'a str {
    match entry {
        DisplayEntry::Single { index, .. } => &messages[*index].session_id,
        DisplayEntry::Paired { request_index, .. } => &messages[*request_index].session_id,
        DisplayEntry::Collapsed { start_index, .. } => &messages[*start_index].session_id,
        DisplayEntry::Scope { parent, .. } => entry_session_id(parent, messages),
    }
}

/// Build a styled [`Line`] for any display entry.
fn render_entry_line(
    entry: &DisplayEntry,
    messages: &[SessionMessage],
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    match entry {
        DisplayEntry::Single { index, .. } => {
            format_message_styled(&messages[*index], icons, theme)
        }
        DisplayEntry::Paired {
            request_index,
            response_index,
            ..
        } => format_pair_styled(
            &messages[*request_index],
            &messages[*response_index],
            icons,
            theme,
        ),
        DisplayEntry::Collapsed {
            start_index,
            end_index,
            count,
            ..
        } => format_collapsed_styled(messages, *start_index, *end_index, *count, icons, theme),
        DisplayEntry::Scope {
            parent,
            children,
            position,
        } => format_scope_styled(parent, children.len(), *position, messages, icons, theme),
    }
}

// ── Rendering ─────────────────────────────────────────────────────────

/// Render the message stream into the given area.
///
/// Each entry line: `XX <formatted content>` where `XX` is the hex
/// badge. Scope/collapsed children use tree-drawing characters with
/// indentation instead of a badge. The scrollbar occupies the rightmost
/// column. The cursor row is highlighted.
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

    // Render visible display rows.
    for row in 0..viewport_height {
        let row_idx = state.scroll_position + row;
        if row_idx >= state.display_rows.len() {
            break;
        }

        let display_row = &state.display_rows[row_idx];
        let line = render_display_row(display_row, state, icons, theme);
        let y = area.y + row as u16;
        buf.set_line(area.x, y, &line, content_width);

        // Highlight cursor row.
        if row_idx == state.cursor {
            for x in area.x..area.x + content_width {
                let cell = &mut buf[(x, y)];
                // Merge selection background without overwriting foreground.
                if let Some(bg) = theme.selection.bg {
                    cell.set_bg(bg);
                } else {
                    // Fallback: use REVERSED modifier.
                    cell.modifier |= ratatui::style::Modifier::REVERSED;
                }
            }
        }
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

/// Build the styled [`Line`] for a single display row.
fn render_display_row(
    row: &DisplayRow,
    state: &StreamState,
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    match row {
        DisplayRow::Entry(entry_idx) => {
            let entry = &state.entries[*entry_idx];
            let session_id = entry_session_id(entry, &state.messages);
            let badge = state.badges.get(session_id);
            let styled = render_entry_line(entry, &state.messages, icons, theme);
            let mut spans = Vec::with_capacity(styled.spans.len() + 1);
            spans.push(Span::styled(format!("{badge} "), theme.accent));
            spans.extend(styled.spans);
            Line::from(spans)
        }
        DisplayRow::ScopeChild {
            entry_idx,
            child_idx,
            is_last,
        } => {
            let entry = &state.entries[*entry_idx];
            let DisplayEntry::Scope { children, .. } = entry else {
                return Line::from("");
            };
            let child = &children[*child_idx];
            let tree_char = if *is_last { TREE_END } else { TREE_MID };
            let styled = render_entry_line(child, &state.messages, icons, theme);
            let mut spans = Vec::with_capacity(styled.spans.len() + 2);
            spans.push(Span::styled(BADGE_INDENT.to_string(), theme.muted));
            spans.push(Span::styled(tree_char.to_string(), theme.muted));
            spans.extend(styled.spans);
            Line::from(spans)
        }
        DisplayRow::ExpandedMessage {
            msg_idx, is_last, ..
        } => {
            let msg = &state.messages[*msg_idx];
            let tree_char = if *is_last { TREE_END } else { TREE_MID };
            let styled = format_message_styled(msg, icons, theme);
            let mut spans = Vec::with_capacity(styled.spans.len() + 2);
            spans.push(Span::styled(BADGE_INDENT.to_string(), theme.muted));
            spans.push(Span::styled(tree_char.to_string(), theme.muted));
            spans.extend(styled.spans);
            Line::from(spans)
        }
    }
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

    fn make_message_with_ids(
        session_id: &str,
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
    ) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            ..test_support::message_with_ids(id, r#type, method, server, request_id, parent_id)
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
    fn test_stream_state_cursor_up_disables_auto() {
        let messages: Vec<_> = (0..20)
            .map(|i| make_message("s1", &format!("method-{i}")))
            .collect();
        let mut state = StreamState::new(messages);
        state.pin_to_bottom(10);

        state.cursor_up(3);
        assert!(!state.auto_scroll);
        assert_eq!(state.cursor, 16); // pin_to_bottom → cursor=19, then 19-3=16
    }

    #[test]
    fn test_stream_state_cursor_down_re_enables_auto() {
        let messages: Vec<_> = (0..20)
            .map(|i| make_message("s1", &format!("method-{i}")))
            .collect();
        let mut state = StreamState::new(messages);
        state.pin_to_bottom(10);
        state.cursor_up(5);

        state.cursor_down(5, 10);
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

    // ── Pipeline wiring tests ────────────────────────────────────────

    #[test]
    fn test_pipeline_produces_entries() {
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "textDocument/definition"),
        ];
        let state = StreamState::new(messages);
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.display_rows.len(), 2);
    }

    #[test]
    fn test_scope_default_expanded() {
        // MCP parent + LSP child → scope entry, default expanded.
        let messages = vec![
            make_message_with_ids("s1", 1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_ids(
                "s1",
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                Some(501),
                Some(500),
            ),
        ];
        let state = StreamState::new(messages);
        // Pipeline: pair_merge produces 2 singles, scope_collapse groups them.
        assert_eq!(state.entries.len(), 1, "expected 1 scope entry");
        // Scope default expanded: header + 1 child = 2 display rows.
        assert_eq!(
            state.display_rows.len(),
            2,
            "scope should be expanded by default"
        );
        assert_eq!(state.display_rows[0], DisplayRow::Entry(0));
        assert_eq!(
            state.display_rows[1],
            DisplayRow::ScopeChild {
                entry_idx: 0,
                child_idx: 0,
                is_last: true,
            }
        );
    }

    #[test]
    fn test_toggle_scope_collapses() {
        let messages = vec![
            make_message_with_ids("s1", 1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_ids(
                "s1",
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                Some(501),
                Some(500),
            ),
            make_message_with_ids(
                "s1",
                3,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                Some(502),
                Some(500),
            ),
        ];
        let mut state = StreamState::new(messages);
        // Default: header + 2 children = 3 rows.
        assert_eq!(state.display_rows.len(), 3);

        // Toggle: collapse the scope.
        state.toggle_expansion();
        assert_eq!(
            state.display_rows.len(),
            1,
            "scope should collapse to header only"
        );

        // Toggle again: re-expand.
        state.toggle_expansion();
        assert_eq!(state.display_rows.len(), 3, "scope should re-expand");
    }

    #[test]
    fn test_toggle_collapsed_run_expands() {
        let messages = vec![
            SessionMessage {
                session_id: "s1".to_string(),
                ..test_support::message_with_payload(
                    "lsp",
                    "$/progress",
                    "rust-analyzer",
                    serde_json::json!({"token": "ra/indexing"}),
                )
            },
            SessionMessage {
                session_id: "s1".to_string(),
                ..test_support::message_with_payload(
                    "lsp",
                    "$/progress",
                    "rust-analyzer",
                    serde_json::json!({"token": "ra/indexing"}),
                )
            },
            SessionMessage {
                session_id: "s1".to_string(),
                ..test_support::message_with_payload(
                    "lsp",
                    "$/progress",
                    "rust-analyzer",
                    serde_json::json!({"token": "ra/indexing"}),
                )
            },
        ];
        let mut state = StreamState::new(messages);
        // Default: collapsed = 1 summary row.
        assert_eq!(state.display_rows.len(), 1);

        // Toggle: expand to header + 3 individual messages.
        state.toggle_expansion();
        assert_eq!(
            state.display_rows.len(),
            4,
            "expanded collapsed run: header + 3 messages"
        );

        // Toggle again: collapse back.
        state.toggle_expansion();
        assert_eq!(state.display_rows.len(), 1);
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

    #[test]
    fn test_render_scope_tree_chars() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let messages = vec![
            make_message_with_ids("s1", 1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_ids(
                "s1",
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                Some(501),
                Some(500),
            ),
            make_message_with_ids(
                "s1",
                3,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                Some(502),
                Some(500),
            ),
        ];
        let state = StreamState::new(messages);

        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());

        let backend = TestBackend::new(80, 10);
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
            content.contains("├─"),
            "expected tree mid char in output, got: {content}"
        );
        assert!(
            content.contains("└─"),
            "expected tree end char in output, got: {content}"
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
