// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Unified message stream with hex badges, scope lifecycle, and scrolling.
//!
//! The stream is the primary view surface for TUI v2: a full-width,
//! scrollable, chronological list of scopes and standalone messages,
//! prefixed with per-session hex badges. Each MCP tool call is a scope
//! that opens on pre-tool hook arrival, streams children live, and
//! auto-collapses to a summary line when the post-tool hook signals
//! completion.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::format::{format_message_styled, format_scope_header_styled};
use super::icons::IconSet;
use super::scope::{Scope, ScopeState, StreamEntry};
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
    /// normal operation — badges are assigned during message routing).
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
/// The stream viewport renders display rows, not raw entries.
/// Scope expansion state determines which rows exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRow {
    /// A scope header line (tool name, state, child count).
    ScopeHeader(usize),
    /// A child within an expanded scope, rendered with tree chars.
    ScopeChild {
        /// Index into `StreamState::entries` for the parent scope.
        entry_idx: usize,
        /// Index into the scope's `children` vec.
        child_idx: usize,
        /// Whether this is the last child (for `└─` vs `├─`).
        is_last: bool,
    },
    /// A standalone message not belonging to any scope.
    Standalone(usize),
}

// ── Stream state ──────────────────────────────────────────────────────

/// Scroll, cursor, scope routing, and viewport state for the message stream.
pub struct StreamState {
    /// All stream entries (scopes and standalone messages) in chronological order.
    pub entries: Vec<StreamEntry>,
    /// Flattened display rows for rendering.
    pub display_rows: Vec<DisplayRow>,
    /// Index of the first visible row in the viewport.
    pub scroll_position: usize,
    /// Whether auto-scroll is active (viewport pinned to bottom).
    pub auto_scroll: bool,
    /// Currently focused display row (for Enter expansion toggle).
    pub cursor: usize,
    /// Hex badge assignment for session IDs.
    pub badges: HexBadgeMap,
    /// `scope_id` → entries index for direct scope children (MCP req/resp, post-hook).
    scope_id_map: HashMap<i64, usize>,
    /// MCP correlation ID → entries index for LSP child routing.
    mcp_corr_map: HashMap<i64, usize>,
    /// Session ID → entries index of the currently active (open) scope.
    active_scope: HashMap<String, usize>,
}

impl StreamState {
    /// Create a new stream state by replaying historical messages through routing.
    #[must_use]
    pub fn new(messages: Vec<SessionMessage>) -> Self {
        let mut state = Self {
            entries: Vec::new(),
            display_rows: Vec::new(),
            scroll_position: 0,
            auto_scroll: true,
            cursor: 0,
            badges: HexBadgeMap::new(),
            scope_id_map: HashMap::new(),
            mcp_corr_map: HashMap::new(),
            active_scope: HashMap::new(),
        };
        for msg in messages {
            state.route_message(msg);
        }
        state.rebuild_display_rows();
        state
    }

    /// Append new messages from a tail reader, routing each into scopes.
    pub fn append(&mut self, messages: Vec<SessionMessage>) {
        for msg in messages {
            self.route_message(msg);
        }
        self.rebuild_display_rows();
    }

    /// Route a single message into the scope model.
    ///
    /// Creates, updates, or closes scopes based on message type and
    /// `parent_id` relationships. Messages that don't belong to any
    /// scope become standalone entries.
    fn route_message(&mut self, msg: SessionMessage) {
        self.badges.badge(&msg.session_id);

        // 1. Pre-tool hook → create new scope.
        if msg.r#type == "hook" && msg.parent_id.is_none() && msg.method.starts_with("pre-tool/") {
            // Abandon any active scope for this session.
            if let Some(&idx) = self.active_scope.get(&msg.session_id)
                && let StreamEntry::Scope(scope) = &mut self.entries[idx]
                && scope.is_active()
            {
                scope.abandon();
            }
            let idx = self.entries.len();
            let scope_id = msg.request_id.unwrap_or(msg.id);
            self.scope_id_map.insert(scope_id, idx);
            self.active_scope.insert(msg.session_id.clone(), idx);
            self.entries
                .push(StreamEntry::Scope(Box::new(Scope::new(msg))));
            return;
        }

        // 2. Route by parent_id.
        if let Some(pid) = msg.parent_id {
            // Check scope_id_map — direct scope children (MCP req/resp, post-hook).
            if let Some(&idx) = self.scope_id_map.get(&pid)
                && let StreamEntry::Scope(scope) = &mut self.entries[idx]
            {
                // Post-tool hook → close scope.
                if msg.r#type == "hook" && msg.method.starts_with("post-tool/") {
                    let session = scope.session_id.clone();
                    scope.close(msg);
                    // Only clear active_scope if it still points to this scope.
                    if self.active_scope.get(&session) == Some(&idx) {
                        self.active_scope.remove(&session);
                    }
                    return;
                }
                // MCP tools/call → request or response.
                if msg.r#type == "mcp" && msg.method == "tools/call" {
                    if scope.request.is_none() {
                        if let Some(corr_id) = msg.request_id {
                            self.mcp_corr_map.insert(corr_id, idx);
                        }
                        scope.attach_request(msg);
                    } else {
                        scope.attach_response(msg);
                    }
                    return;
                }
                // Generic child of the scope (e.g., hook child).
                scope.children.push(msg);
                return;
            }

            // Check mcp_corr_map — LSP children of the tool call.
            if let Some(&idx) = self.mcp_corr_map.get(&pid)
                && let StreamEntry::Scope(scope) = &mut self.entries[idx]
            {
                scope.children.push(msg);
                return;
            }
        }

        // 3. Standalone message (no matching scope).
        self.entries.push(StreamEntry::Standalone(msg));
    }

    /// Flatten entries into display rows based on scope expansion state.
    fn rebuild_display_rows(&mut self) {
        self.display_rows.clear();
        for (i, entry) in self.entries.iter().enumerate() {
            match entry {
                StreamEntry::Scope(scope) => {
                    self.display_rows.push(DisplayRow::ScopeHeader(i));
                    if scope.is_expanded() {
                        let len = scope.children.len();
                        for ci in 0..len {
                            self.display_rows.push(DisplayRow::ScopeChild {
                                entry_idx: i,
                                child_idx: ci,
                                is_last: ci == len - 1,
                            });
                        }
                    }
                }
                StreamEntry::Standalone(_) => {
                    self.display_rows.push(DisplayRow::Standalone(i));
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

    /// Toggle expansion state on the scope at the cursor position.
    ///
    /// Closed/abandoned scopes toggle between summary (header only) and
    /// expanded (header + children). Active scopes are always expanded.
    /// Standalone messages are not expandable.
    pub fn toggle_expansion(&mut self) {
        let Some(row) = self.display_rows.get(self.cursor) else {
            return;
        };
        let entry_idx = match row {
            DisplayRow::ScopeHeader(idx) | DisplayRow::ScopeChild { entry_idx: idx, .. } => *idx,
            DisplayRow::Standalone(_) => return,
        };
        if let StreamEntry::Scope(scope) = &mut self.entries[entry_idx]
            && matches!(scope.state, ScopeState::Closed | ScopeState::Abandoned)
        {
            scope.user_expanded = !scope.user_expanded;
            self.rebuild_display_rows();
        }
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

// ── Rendering ─────────────────────────────────────────────────────────

/// Render the message stream into the given area.
///
/// Each entry line: `XX <formatted content>` where `XX` is the hex
/// badge. Scope children use tree-drawing characters with indentation
/// instead of a badge. The scrollbar occupies the rightmost column.
/// The cursor row is highlighted.
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
        DisplayRow::ScopeHeader(entry_idx) => {
            let StreamEntry::Scope(scope) = &state.entries[*entry_idx] else {
                return Line::from("");
            };
            let badge = state.badges.get(&scope.session_id);
            let styled = format_scope_header_styled(scope, icons, theme);
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
            let StreamEntry::Scope(scope) = &state.entries[*entry_idx] else {
                return Line::from("");
            };
            let child = &scope.children[*child_idx];
            let tree_char = if *is_last { TREE_END } else { TREE_MID };
            let styled = format_message_styled(child, icons, theme);
            let mut spans = Vec::with_capacity(styled.spans.len() + 2);
            spans.push(Span::styled(BADGE_INDENT.to_string(), theme.muted));
            spans.push(Span::styled(tree_char.to_string(), theme.muted));
            spans.extend(styled.spans);
            Line::from(spans)
        }
        DisplayRow::Standalone(entry_idx) => {
            let StreamEntry::Standalone(msg) = &state.entries[*entry_idx] else {
                return Line::from("");
            };
            let badge = state.badges.get(&msg.session_id);
            let styled = format_message_styled(msg, icons, theme);
            let mut spans = Vec::with_capacity(styled.spans.len() + 1);
            spans.push(Span::styled(format!("{badge} "), theme.accent));
            spans.extend(styled.spans);
            Line::from(spans)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
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

    /// Build a pre-tool hook message that creates a scope.
    fn pre_hook(session_id: &str, scope_id: i64) -> SessionMessage {
        make_message_with_ids(
            session_id,
            100 + scope_id,
            "hook",
            "pre-tool/editing-state",
            "",
            Some(scope_id),
            None,
        )
    }

    /// Build an MCP request that attaches to a scope.
    fn mcp_request(session_id: &str, scope_id: i64, tool: &str) -> SessionMessage {
        let corr_id = scope_id + 1000;
        SessionMessage {
            payload: serde_json::json!({"params": {"name": tool}}),
            ..make_message_with_ids(
                session_id,
                200 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(corr_id),
                Some(scope_id),
            )
        }
    }

    /// Build an MCP response for a scope.
    fn mcp_response(session_id: &str, scope_id: i64) -> SessionMessage {
        let corr_id = scope_id + 1000;
        SessionMessage {
            payload: serde_json::json!({"result": {"content": [{"type": "text", "text": "ok"}]}}),
            ..make_message_with_ids(
                session_id,
                300 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(corr_id),
                Some(scope_id),
            )
        }
    }

    /// Build a post-tool hook that closes a scope.
    fn post_hook(session_id: &str, scope_id: i64) -> SessionMessage {
        make_message_with_ids(
            session_id,
            400 + scope_id,
            "hook",
            "post-tool/diagnostics",
            "",
            Some(scope_id + 2000),
            Some(scope_id),
        )
    }

    /// Build an LSP child message of an MCP tool call.
    fn lsp_child(session_id: &str, scope_id: i64, method: &str) -> SessionMessage {
        let mcp_corr_id = scope_id + 1000;
        make_message_with_ids(
            session_id,
            500 + scope_id,
            "lsp",
            method,
            "rust-analyzer",
            Some(mcp_corr_id + 100),
            Some(mcp_corr_id),
        )
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

    // ── Scope routing tests ──────────────────────────────────────────

    #[test]
    fn test_scope_full_lifecycle() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
            mcp_response("s1", 1),
            post_hook("s1", 1),
        ];
        let state = StreamState::new(messages);

        // Should produce one scope entry.
        assert_eq!(state.entries.len(), 1, "expected 1 scope");
        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected Scope entry");
        };
        assert_eq!(scope.state, ScopeState::Closed);
        assert!(scope.request.is_some());
        assert!(scope.response.is_some());
        assert!(scope.post_hook.is_some());
        assert_eq!(scope.children.len(), 2);
        // Closed scope: collapsed to header only.
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_scope_open_expanded() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 1);
        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected Scope entry");
        };
        assert_eq!(scope.state, ScopeState::Open);
        // Open scope: header + 1 child = 2 rows.
        assert_eq!(state.display_rows.len(), 2);
    }

    #[test]
    fn test_scope_settling_expanded() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            mcp_response("s1", 1),
        ];
        let state = StreamState::new(messages);

        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected Scope entry");
        };
        assert_eq!(scope.state, ScopeState::Settling);
        // Settling: still expanded (header + 1 child).
        assert_eq!(state.display_rows.len(), 2);
    }

    #[test]
    fn test_scope_abandoned_on_new_prehook() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            // New pre-tool hook for same session without closing old scope.
            pre_hook("s1", 2),
            mcp_request("s1", 2, "glob"),
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 2);
        let StreamEntry::Scope(scope1) = &state.entries[0] else {
            panic!("expected first scope");
        };
        assert_eq!(scope1.state, ScopeState::Abandoned);
        assert!(!scope1.is_expanded(), "abandoned scope should be collapsed");

        let StreamEntry::Scope(scope2) = &state.entries[1] else {
            panic!("expected second scope");
        };
        assert_eq!(scope2.state, ScopeState::Open);
    }

    #[test]
    fn test_standalone_messages() {
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "textDocument/definition"),
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 2);
        assert!(matches!(state.entries[0], StreamEntry::Standalone(_)));
        assert!(matches!(state.entries[1], StreamEntry::Standalone(_)));
        assert_eq!(state.display_rows.len(), 2);
    }

    #[test]
    fn test_mixed_scopes_and_standalones() {
        let messages = vec![
            make_message("s1", "initialize"), // standalone
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            mcp_response("s1", 1),
            post_hook("s1", 1),
            make_message("s1", "shutdown"), // standalone
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 3, "standalone + scope + standalone");
        assert!(matches!(state.entries[0], StreamEntry::Standalone(_)));
        assert!(matches!(state.entries[1], StreamEntry::Scope(_)));
        assert!(matches!(state.entries[2], StreamEntry::Standalone(_)));
        // Closed scope = 1 row, 2 standalones = 2 rows, total 3.
        assert_eq!(state.display_rows.len(), 3);
    }

    #[test]
    fn test_two_sessions_independent_scopes() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            pre_hook("s2", 2),
            mcp_request("s2", 2, "glob"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s2", 2, "textDocument/references"),
            mcp_response("s1", 1),
            post_hook("s1", 1),
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 2);
        // Scope 1 is closed, scope 2 is open.
        let StreamEntry::Scope(s1) = &state.entries[0] else {
            panic!("expected scope 1");
        };
        assert_eq!(s1.state, ScopeState::Closed);
        assert_eq!(s1.children.len(), 1);

        let StreamEntry::Scope(s2) = &state.entries[1] else {
            panic!("expected scope 2");
        };
        assert_eq!(s2.state, ScopeState::Open);
        assert_eq!(s2.children.len(), 1);
    }

    #[test]
    fn test_append_routes_incrementally() {
        let mut state = StreamState::new(vec![pre_hook("s1", 1), mcp_request("s1", 1, "grep")]);
        assert_eq!(state.entries.len(), 1);

        state.append(vec![
            lsp_child("s1", 1, "workspace/symbol"),
            mcp_response("s1", 1),
            post_hook("s1", 1),
        ]);

        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected scope");
        };
        assert_eq!(scope.state, ScopeState::Closed);
        assert_eq!(scope.children.len(), 1);
        // Closed: header only.
        assert_eq!(state.display_rows.len(), 1);
    }

    // ── Expansion toggle tests ──────────────────────────────────────

    #[test]
    fn test_toggle_closed_scope_expands() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
            mcp_response("s1", 1),
            post_hook("s1", 1),
        ];
        let mut state = StreamState::new(messages);
        // Closed: header only.
        assert_eq!(state.display_rows.len(), 1);

        // Toggle: expand.
        state.toggle_expansion();
        assert_eq!(
            state.display_rows.len(),
            3,
            "expanded scope: header + 2 children"
        );

        // Toggle: collapse.
        state.toggle_expansion();
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_toggle_open_scope_noop() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
        ];
        let mut state = StreamState::new(messages);
        // Open scope: header + 1 child = 2 rows.
        assert_eq!(state.display_rows.len(), 2);

        // Toggle on open scope: no-op.
        state.toggle_expansion();
        assert_eq!(
            state.display_rows.len(),
            2,
            "open scope cannot be collapsed"
        );
    }

    #[test]
    fn test_toggle_on_child_toggles_parent() {
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
            mcp_response("s1", 1),
            post_hook("s1", 1),
        ];
        let mut state = StreamState::new(messages);
        // Expand closed scope first.
        state.toggle_expansion();
        assert_eq!(state.display_rows.len(), 3);

        // Move cursor to a child row and toggle.
        state.cursor = 1;
        state.toggle_expansion();
        assert_eq!(
            state.display_rows.len(),
            1,
            "toggling on child should collapse parent"
        );
    }

    #[test]
    fn test_toggle_standalone_noop() {
        let messages = vec![make_message("s1", "textDocument/hover")];
        let mut state = StreamState::new(messages);
        assert_eq!(state.display_rows.len(), 1);

        state.toggle_expansion();
        assert_eq!(state.display_rows.len(), 1, "standalone not expandable");
    }

    // ── Scroll / cursor tests ────────────────────────────────────────

    #[test]
    fn test_stream_state_auto_scroll() {
        let messages: Vec<_> = (0..20)
            .map(|i| make_message("s1", &format!("method-{i}")))
            .collect();
        let state = StreamState::new(messages);
        assert!(state.auto_scroll);
        assert_eq!(state.scroll_position, 0);
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
        assert_eq!(state.cursor, 16);
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

        // Open scope with 2 children shows tree chars.
        let messages = vec![
            pre_hook("s1", 1),
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
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
