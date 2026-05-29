// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Unified message stream with hex badges, scope lifecycle, and scrolling.
//!
//! The stream is the primary view surface for TUI v2: a full-width,
//! scrollable, chronological list of scopes and standalone messages,
//! prefixed with per-session hex badges. All messages sharing a
//! `parent_id` UUID are grouped into a scope: the first message is the
//! request (header), the response closes the scope, and everything in
//! between is a child.

use std::collections::{HashMap, HashSet};

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
    /// A detail line within an expanded standalone internal message.
    StandaloneDetail {
        /// Index into `StreamState::entries` for the parent standalone.
        entry_idx: usize,
        /// Index into the detail lines vec.
        detail_idx: usize,
        /// Whether this is the last detail line.
        is_last: bool,
    },
}

// ── Paging ───────────────────────────────────────────────────────────

/// Default number of scope roots per page.
pub const PAGE_SIZE: usize = 50;

/// Prefetch buffer zone — trigger a fetch when the cursor is within
/// this many display rows of the loaded boundary.
const BUFFER_ZONE: usize = 20;

/// A request for the caller to fetch a new page of scopes.
#[derive(Debug)]
pub enum PageRequest {
    /// Load older scopes, prepending to the loaded range.
    /// Contains the oldest loaded scope root message ID.
    Older(i64),
}

/// Get the scope root message ID for a stream entry.
///
/// For scopes, this is the request message's ID. For standalones, it's
/// the message ID itself.
fn entry_root_id(entry: &StreamEntry) -> i64 {
    match entry {
        StreamEntry::Scope(scope) => scope.request.id,
        StreamEntry::Standalone(msg) => msg.id,
    }
}

/// Determine whether a message is the response that closes a scope.
///
/// - **Hook:** second hook message in the scope (request was also a hook).
/// - **MCP:** outgoing message with `result` or `error` key, no `method`.
fn is_scope_response(msg: &SessionMessage, scope: &Scope) -> bool {
    match msg.r#type.as_str() {
        "hook" => scope.request.r#type == "hook",
        "mcp" => {
            let p = &msg.payload;
            !p.get("method").is_some_and(serde_json::Value::is_string)
                && (p.get("result").is_some() || p.get("error").is_some())
        }
        _ => false,
    }
}

/// Whether a message is server noise that belongs in the sidebar, not
/// the stream.
///
/// Server noise: `$/progress`, `window/logMessage`, `window/showMessage`
/// with no `parent_id` (not part of a scope). These are server-level
/// status messages shown in the sidebar server dashboard.
fn is_server_noise(msg: &SessionMessage) -> bool {
    msg.parent_id.is_none()
        && msg.r#type == "lsp"
        && super::data::SERVER_NOISE_METHODS.contains(&msg.method.as_str())
}

/// Route a single message into entries and scope map.
///
/// Used by both instance methods (live append) and page operations
/// (routing into temporary buffers). Server noise (`$/progress`,
/// `window/logMessage`, `window/showMessage` without `parent_id`)
/// is silently dropped — it is shown in the sidebar instead.
fn route_into(
    entries: &mut Vec<StreamEntry>,
    scope_map: &mut HashMap<String, usize>,
    msg: SessionMessage,
) {
    // Server noise belongs in the sidebar, not the stream.
    if is_server_noise(&msg) {
        return;
    }

    let Some(ref pid) = msg.parent_id else {
        entries.push(StreamEntry::Standalone(msg));
        return;
    };

    if let Some(&idx) = scope_map.get(pid.as_str()) {
        let StreamEntry::Scope(scope) = &mut entries[idx] else {
            entries.push(StreamEntry::Standalone(msg));
            return;
        };
        if is_scope_response(&msg, scope) {
            scope.close(msg);
        } else {
            scope.children.push(msg);
        }
    } else {
        let idx = entries.len();
        scope_map.insert(pid.clone(), idx);
        entries.push(StreamEntry::Scope(Box::new(Scope::new(msg))));
    }
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
    /// `parent_id` UUID → entries index. Single routing map.
    scope_map: HashMap<String, usize>,
    /// Entry indices of expanded standalone internal messages.
    expanded_standalones: HashSet<usize>,
    /// Visual selection anchor (display row index). When `Some`, the
    /// selection spans from `visual_anchor` to `cursor` (inclusive).
    /// `None` means no visual selection is active.
    visual_anchor: Option<usize>,

    // ── Filtering state ──────────────────────────────────────────────
    /// Active session filter. `None` = show all. `Some(set)` = show only
    /// entries whose session belongs to the set. Daemon-level events (no
    /// matching session) are hidden when a filter is active.
    session_filter: Option<HashSet<String>>,
    /// Active server filter. `None` = show all. `Some(set)` = show only
    /// scopes whose LSP children involve a server instance in the set.
    /// Each key is `(server_name, scope_root)`. Scopes with no matching
    /// LSP children are hidden.
    server_filter: Option<HashSet<super::sidebar::ServerInstanceKey>>,

    // ── Paging state ─────────────────────────────────────────────────
    /// Whether all older scopes have been loaded (reached the beginning).
    pub reached_beginning: bool,

    // ── Search state ────────────────────────────────────────────────
    /// Active search query (case-insensitive substring match).
    search_query: Option<String>,
    /// Display row indices that match the current search query.
    search_matches: Vec<usize>,
    /// Index into `search_matches` for the current match.
    /// `None` when there are no matches.
    search_match_idx: Option<usize>,
    /// Whether search matches need recomputing after a display row rebuild.
    search_dirty: bool,
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
            scope_map: HashMap::new(),
            expanded_standalones: HashSet::new(),
            visual_anchor: None,
            session_filter: None,
            server_filter: None,
            reached_beginning: false,
            search_query: None,
            search_matches: Vec::new(),
            search_match_idx: None,
            search_dirty: false,
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
    fn route_message(&mut self, msg: SessionMessage) {
        self.badges.badge(&msg.session_id);
        route_into(&mut self.entries, &mut self.scope_map, msg);
    }

    /// Flatten entries into display rows based on scope expansion state
    /// and active session/server filters.
    fn rebuild_display_rows(&mut self) {
        self.display_rows.clear();

        for (i, entry) in self.entries.iter().enumerate() {
            // Apply session filter: skip entries not in the selected set.
            if let Some(ref filter) = self.session_filter
                && !filter.contains(entry.session_id())
            {
                continue;
            }

            // Apply server filter: scope must have LSP children involving
            // a selected server instance. Scopes with no matching LSP
            // children (including hook-only scopes) are hidden.
            if let Some(ref server_set) = self.server_filter {
                let matches = match entry {
                    StreamEntry::Scope(scope) => scope.children.iter().any(|c| {
                        c.r#type == "lsp"
                            && server_set.contains(&(c.server.clone(), c.scope_root.clone()))
                    }),
                    StreamEntry::Standalone(msg) => {
                        msg.r#type == "lsp"
                            && server_set.contains(&(msg.server.clone(), msg.scope_root.clone()))
                    }
                };
                if !matches {
                    continue;
                }
            }

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
                StreamEntry::Standalone(msg) => {
                    self.display_rows.push(DisplayRow::Standalone(i));
                    if self.expanded_standalones.contains(&i) && msg.r#type == "internal" {
                        let details = super::format::internal_detail_lines(msg);
                        let len = details.len();
                        for di in 0..len {
                            self.display_rows.push(DisplayRow::StandaloneDetail {
                                entry_idx: i,
                                detail_idx: di,
                                is_last: di == len - 1,
                            });
                        }
                    }
                }
            }
        }
        // Clamp cursor and scroll to valid range.
        let max = self.display_rows.len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
        let scroll_max = self.display_rows.len().saturating_sub(1);
        self.scroll_position = self.scroll_position.min(scroll_max);

        // Mark search matches as stale when a query is active.
        if self.search_query.is_some() {
            self.search_dirty = true;
        }
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
        // Re-enable auto-scroll if cursor reached the bottom (but not
        // during visual selection — the cursor should stay put).
        let content_max = self.row_count().saturating_sub(viewport_height);
        if self.cursor >= self.row_count().saturating_sub(1) && self.visual_anchor.is_none() {
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
        // Re-enable auto-scroll if we've reached the bottom (but not
        // during visual selection).
        if self.scroll_position >= max && self.visual_anchor.is_none() {
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
    /// Suppressed during visual selection to prevent the cursor from
    /// drifting to newly appended rows.
    pub const fn apply_auto_scroll(&mut self, viewport_height: usize) {
        if self.auto_scroll && self.visual_anchor.is_none() {
            self.scroll_position = self.row_count().saturating_sub(viewport_height);
            self.cursor = self.row_count().saturating_sub(1);
        }
    }

    /// Toggle expansion state on the scope or standalone at the cursor.
    ///
    /// Closed scopes toggle between summary (header only) and expanded
    /// (header + children). Open scopes are always expanded.
    /// Standalone internal messages toggle to show payload detail lines.
    pub fn toggle_expansion(&mut self) {
        let Some(row) = self.display_rows.get(self.cursor) else {
            return;
        };
        match row {
            DisplayRow::ScopeHeader(idx) | DisplayRow::ScopeChild { entry_idx: idx, .. } => {
                let idx = *idx;
                if let StreamEntry::Scope(scope) = &mut self.entries[idx]
                    && scope.state == ScopeState::Closed
                {
                    scope.user_expanded = !scope.user_expanded;
                    self.rebuild_display_rows();
                }
            }
            DisplayRow::Standalone(idx) | DisplayRow::StandaloneDetail { entry_idx: idx, .. } => {
                let idx = *idx;
                if let StreamEntry::Standalone(msg) = &self.entries[idx]
                    && msg.r#type == "internal"
                {
                    if self.expanded_standalones.contains(&idx) {
                        self.expanded_standalones.remove(&idx);
                    } else {
                        self.expanded_standalones.insert(idx);
                    }
                    self.rebuild_display_rows();
                }
            }
        }
    }

    // ── Visual selection ───────────────────────────────────────────

    /// Enter visual selection mode, anchoring at the current cursor.
    pub const fn start_visual(&mut self) {
        self.visual_anchor = Some(self.cursor);
    }

    /// Exit visual selection mode.
    pub const fn exit_visual(&mut self) {
        self.visual_anchor = None;
    }

    /// Whether visual selection mode is active.
    #[must_use]
    pub const fn in_visual(&self) -> bool {
        self.visual_anchor.is_some()
    }

    /// Return the inclusive display-row range of the visual selection.
    ///
    /// Returns `(start, end)` where `start <= end`. Returns `None` when
    /// visual mode is inactive.
    #[must_use]
    pub const fn visual_range(&self) -> Option<(usize, usize)> {
        let Some(anchor) = self.visual_anchor else {
            return None;
        };
        if anchor <= self.cursor {
            Some((anchor, self.cursor))
        } else {
            Some((self.cursor, anchor))
        }
    }

    // ── Yank ────────────────────────────────────────────────────────

    /// Get plain-text content for the current yank target.
    ///
    /// In visual mode, returns all rows in the selection range joined
    /// by newlines. Otherwise returns the single row at the cursor.
    /// Returns `None` if the cursor is out of range.
    #[must_use]
    pub fn yank_text(&self, icons: &super::icons::IconSet) -> Option<String> {
        if let Some((start, end)) = self.visual_range() {
            let lines: Vec<String> = (start..=end)
                .filter_map(|i| self.row_plain_text(i, icons))
                .collect();
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        } else {
            self.row_plain_text(self.cursor, icons)
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

    // ── Session filter ────────────────────────────────────────────

    /// Update the session filter and rebuild display rows.
    ///
    /// `None` = show all. `Some(set)` = show only entries belonging to
    /// sessions in the set. Daemon-level events are hidden when a
    /// filter is active.
    pub fn set_session_filter(&mut self, filter: Option<HashSet<String>>) {
        self.session_filter = filter;
        self.rebuild_display_rows();
    }

    /// Update the server filter and rebuild display rows.
    ///
    /// `None` = show all. `Some(set)` = show only scopes whose LSP
    /// children involve a server instance in the set.
    pub fn set_server_filter(
        &mut self,
        filter: Option<HashSet<super::sidebar::ServerInstanceKey>>,
    ) {
        self.server_filter = filter;
        self.rebuild_display_rows();
    }

    // ── Search ─────────────────────────────────────────────────────

    /// Plain-text content for a display row at the given index.
    ///
    /// Used by search matching and yank. Returns `None` for out-of-range
    /// indices or malformed entries.
    fn row_plain_text(&self, row_idx: usize, icons: &super::icons::IconSet) -> Option<String> {
        let row = self.display_rows.get(row_idx)?;
        match row {
            DisplayRow::ScopeHeader(entry_idx) => {
                let StreamEntry::Scope(scope) = &self.entries[*entry_idx] else {
                    return None;
                };
                let badge = self.badges.get(&scope.session_id);
                let plain = super::format::format_scope_header_plain(scope, icons);
                Some(format!("{badge} {plain}"))
            }
            DisplayRow::ScopeChild {
                entry_idx,
                child_idx,
                ..
            } => {
                let StreamEntry::Scope(scope) = &self.entries[*entry_idx] else {
                    return None;
                };
                let child = scope.children.get(*child_idx)?;
                Some(super::format::format_message_plain(child))
            }
            DisplayRow::Standalone(entry_idx) => {
                let StreamEntry::Standalone(msg) = &self.entries[*entry_idx] else {
                    return None;
                };
                let badge = self.badges.get(&msg.session_id);
                let plain = super::format::format_message_plain(msg);
                Some(format!("{badge} {plain}"))
            }
            DisplayRow::StandaloneDetail {
                entry_idx,
                detail_idx,
                ..
            } => {
                let StreamEntry::Standalone(msg) = &self.entries[*entry_idx] else {
                    return None;
                };
                let details = super::format::internal_detail_lines(msg);
                details
                    .get(*detail_idx)
                    .map(|(label, value)| format!("{label}: {value}"))
            }
        }
    }

    /// Set the search query and recompute matches.
    ///
    /// If `query` is `None` or empty, clears the search. Otherwise
    /// performs case-insensitive substring matching on every display
    /// row's plain-text content and jumps to the first match at or
    /// after the current cursor.
    pub fn set_search(&mut self, query: Option<String>, icons: &super::icons::IconSet) {
        self.search_query = query.filter(|q| !q.is_empty());
        self.scan_search_matches(icons);
        self.search_dirty = false;

        if self.search_matches.is_empty() {
            return;
        }

        // Jump to the first match at or after the current cursor.
        let pos = self.search_matches.partition_point(|&m| m < self.cursor);
        self.search_match_idx = Some(if pos < self.search_matches.len() {
            pos
        } else {
            0
        });
        self.cursor = self.search_matches[self.search_match_idx.unwrap_or(0)];
        self.auto_scroll = false;
    }

    /// Recompute search matches if display rows changed since the last scan.
    ///
    /// Preserves cursor position and keeps `search_match_idx` pointing at
    /// the closest match to the current cursor. Called from the event loop
    /// where icons are available.
    pub fn recompute_search_if_dirty(&mut self, icons: &super::icons::IconSet) {
        if !self.search_dirty {
            return;
        }
        self.search_dirty = false;

        let prev_cursor = self.cursor;
        self.scan_search_matches(icons);

        if self.search_matches.is_empty() {
            return;
        }

        // Point search_match_idx at the closest match to the cursor.
        let pos = self.search_matches.partition_point(|&m| m < prev_cursor);
        self.search_match_idx = Some(if pos < self.search_matches.len() {
            pos
        } else {
            self.search_matches.len() - 1
        });
    }

    /// Scan all display rows for the current search query.
    ///
    /// Populates `search_matches` and resets `search_match_idx`.
    fn scan_search_matches(&mut self, icons: &super::icons::IconSet) {
        self.search_matches.clear();
        self.search_match_idx = None;

        let Some(ref pattern) = self.search_query else {
            return;
        };
        let lower = pattern.to_lowercase();

        for idx in 0..self.display_rows.len() {
            if let Some(text) = self.row_plain_text(idx, icons)
                && text.to_lowercase().contains(&lower)
            {
                self.search_matches.push(idx);
            }
        }
    }

    /// Jump to the next search match (wraps around).
    pub fn search_next(&mut self, viewport_height: usize) {
        if self.search_matches.is_empty() {
            return;
        }
        let idx = self.search_match_idx.map_or(0, |i| {
            if i + 1 < self.search_matches.len() {
                i + 1
            } else {
                0
            }
        });
        self.search_match_idx = Some(idx);
        self.cursor = self.search_matches[idx];
        self.auto_scroll = false;

        // Scroll to keep cursor visible.
        if self.cursor < self.scroll_position {
            self.scroll_position = self.cursor;
        } else if self.cursor >= self.scroll_position + viewport_height {
            self.scroll_position = self.cursor.saturating_sub(viewport_height / 2);
        }
    }

    /// Jump to the previous search match (wraps around).
    pub fn search_prev(&mut self, viewport_height: usize) {
        if self.search_matches.is_empty() {
            return;
        }
        let idx = self.search_match_idx.map_or(0, |i| {
            if i > 0 {
                i - 1
            } else {
                self.search_matches.len() - 1
            }
        });
        self.search_match_idx = Some(idx);
        self.cursor = self.search_matches[idx];
        self.auto_scroll = false;

        // Scroll to keep cursor visible.
        if self.cursor < self.scroll_position {
            self.scroll_position = self.cursor;
        } else if self.cursor >= self.scroll_position + viewport_height {
            self.scroll_position = self.cursor.saturating_sub(viewport_height / 2);
        }
    }

    /// Clear the search query and all match state.
    pub fn clear_search(&mut self) {
        self.search_query = None;
        self.search_matches.clear();
        self.search_match_idx = None;
    }

    /// Whether the given display row index is a search match.
    #[must_use]
    pub fn is_search_match(&self, row_idx: usize) -> bool {
        self.search_query.is_some() && self.search_matches.binary_search(&row_idx).is_ok()
    }

    /// Whether a search query is active (has matches to navigate).
    #[must_use]
    pub const fn has_search(&self) -> bool {
        self.search_query.is_some()
    }

    /// The current search query string, if any.
    #[must_use]
    pub fn search_query(&self) -> Option<&str> {
        self.search_query.as_deref()
    }

    /// Format the match counter string (e.g., `"3/17"`).
    ///
    /// Returns `None` if there is no active search.
    #[must_use]
    pub fn search_status(&self) -> Option<String> {
        self.search_query.as_ref()?;
        let total = self.search_matches.len();
        if total == 0 {
            return Some("no matches".to_string());
        }
        let current = self.search_match_idx.map_or(0, |i| i + 1);
        Some(format!("{current}/{total}"))
    }

    // ── Paging ──────────────────────────────────────────────────────

    /// Oldest scope root message ID among all loaded entries.
    #[must_use]
    pub fn oldest_loaded_root(&self) -> Option<i64> {
        self.entries.first().map(entry_root_id)
    }

    /// Prepend a page of older scopes at the beginning of entries.
    ///
    /// If `messages` is empty, marks `reached_beginning` and returns.
    pub fn prepend_page(&mut self, messages: Vec<SessionMessage>) {
        if messages.is_empty() {
            self.reached_beginning = true;
            return;
        }

        let mut temp = Vec::new();
        let mut temp_map: HashMap<String, usize> = HashMap::new();
        for msg in messages {
            self.badges.badge(&msg.session_id);
            route_into(&mut temp, &mut temp_map, msg);
        }

        if temp.is_empty() {
            self.reached_beginning = true;
            return;
        }

        let n = temp.len();

        // Shift existing scope_map indices to make room.
        for idx in self.scope_map.values_mut() {
            *idx += n;
        }
        self.scope_map.extend(temp_map);

        // Shift expanded standalone indices.
        self.expanded_standalones = self.expanded_standalones.iter().map(|i| i + n).collect();

        // Prepend.
        temp.append(&mut self.entries);
        self.entries = temp;

        // Keep cursor and scroll on the same content.
        self.cursor += n;
        self.scroll_position += n;

        self.rebuild_display_rows();
    }

    /// Check whether the cursor is near a paging boundary.
    ///
    /// Returns `Some(PageRequest)` if the caller should fetch more
    /// data, or `None` if no prefetch is needed. Suppressed during
    /// visual selection to prevent content shifts under the selection.
    #[must_use]
    pub fn check_paging(&self) -> Option<PageRequest> {
        if self.display_rows.is_empty() || self.visual_anchor.is_some() {
            return None;
        }

        // Near the top of all loaded data — load older page.
        if self.cursor < BUFFER_ZONE
            && !self.reached_beginning
            && let Some(oldest) = self.oldest_loaded_root()
        {
            return Some(PageRequest::Older(oldest));
        }

        None
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

    // Visual selection range (inclusive), if active.
    let visual = state.visual_range();

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

        // Highlight search matches (non-cursor rows).
        if row_idx != state.cursor
            && state.is_search_match(row_idx)
            && let Some(bg) = theme.search_match.bg
        {
            for x in area.x..area.x + content_width {
                buf[(x, y)].set_bg(bg);
            }
        }

        // Highlight: cursor row, or any row in the visual selection range.
        let highlighted = if let Some((start, end)) = visual {
            row_idx >= start && row_idx <= end
        } else {
            row_idx == state.cursor
        };
        if highlighted {
            for x in area.x..area.x + content_width {
                let cell = &mut buf[(x, y)];
                if let Some(bg) = theme.selection.bg {
                    cell.set_bg(bg);
                } else {
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
        DisplayRow::StandaloneDetail {
            entry_idx,
            detail_idx,
            is_last,
        } => {
            let StreamEntry::Standalone(msg) = &state.entries[*entry_idx] else {
                return Line::from("");
            };
            let details = super::format::internal_detail_lines(msg);
            let Some((label, value)) = details.get(*detail_idx) else {
                return Line::from("");
            };
            let tree_char = if *is_last { TREE_END } else { TREE_MID };
            Line::from(vec![
                Span::styled(BADGE_INDENT.to_string(), theme.muted),
                Span::styled(tree_char.to_string(), theme.muted),
                Span::styled(format!("{label}: "), theme.muted),
                Span::styled(value.clone(), theme.text),
            ])
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

    /// Standalone message with no `parent_id`.
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
        parent_id: Option<&str>,
    ) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            ..test_support::message_with_ids(id, r#type, method, server, parent_id)
        }
    }

    /// MCP request — first message in a scope, creates the scope.
    fn mcp_request(session_id: &str, scope_id: i64, tool: &str) -> SessionMessage {
        SessionMessage {
            payload: serde_json::json!({"params": {"name": tool}}),
            ..make_message_with_ids(
                session_id,
                200 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(&format!("scope-{scope_id}")),
            )
        }
    }

    /// MCP response — closes the scope (has `result`, no `method`).
    fn mcp_response(session_id: &str, scope_id: i64) -> SessionMessage {
        SessionMessage {
            payload: serde_json::json!({"result": {"content": [{"type": "text", "text": "ok"}]}}),
            ..make_message_with_ids(
                session_id,
                300 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(&format!("scope-{scope_id}")),
            )
        }
    }

    /// Hook request — first hook message for a scope UUID.
    fn hook_request(session_id: &str, scope_id: i64) -> SessionMessage {
        make_message_with_ids(
            session_id,
            100 + scope_id,
            "hook",
            "pre-tool/editing-state",
            "",
            Some(&format!("hook-{scope_id}")),
        )
    }

    /// Hook response — second hook message, closes the hook scope.
    fn hook_response(session_id: &str, scope_id: i64) -> SessionMessage {
        make_message_with_ids(
            session_id,
            400 + scope_id,
            "hook",
            "pre-tool/editing-state",
            "",
            Some(&format!("hook-{scope_id}")),
        )
    }

    /// LSP child message sharing the scope's `parent_id`.
    fn lsp_child(session_id: &str, scope_id: i64, method: &str) -> SessionMessage {
        make_message_with_ids(
            session_id,
            500 + scope_id,
            "lsp",
            method,
            "rust-analyzer",
            Some(&format!("scope-{scope_id}")),
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
    fn test_mcp_scope_full_lifecycle() {
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
            mcp_response("s1", 1),
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 1, "expected 1 scope");
        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected Scope entry");
        };
        assert_eq!(scope.state, ScopeState::Closed);
        assert_eq!(scope.request.r#type, "mcp");
        assert!(scope.response.is_some());
        assert_eq!(scope.children.len(), 2);
        // Closed scope: collapsed to header only.
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_scope_open_expanded() {
        let messages = vec![
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
    fn test_hook_request_response_creates_leaf_scope() {
        let messages = vec![hook_request("s1", 1), hook_response("s1", 1)];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 1, "hook pair = 1 scope");
        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected Scope entry");
        };
        assert_eq!(scope.state, ScopeState::Closed);
        assert!(scope.children.is_empty(), "hook scopes are leaf scopes");
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
            make_message("s1", "initialize"), // standalone (no parent_id)
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            mcp_response("s1", 1),
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
            mcp_request("s1", 1, "grep"),
            mcp_request("s2", 2, "glob"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s2", 2, "textDocument/references"),
            mcp_response("s1", 1),
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
        let mut state = StreamState::new(vec![mcp_request("s1", 1, "grep")]);
        assert_eq!(state.entries.len(), 1);

        state.append(vec![
            lsp_child("s1", 1, "workspace/symbol"),
            mcp_response("s1", 1),
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
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
            mcp_response("s1", 1),
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
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
            mcp_response("s1", 1),
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

    // ── IPC scope tests ────────────────────────────────────────────────

    /// IPC request — hook-typed message that opens a scope (e.g.,
    /// `tool/editing-stop` incoming).
    fn ipc_request(session_id: &str, scope_id: i64) -> SessionMessage {
        make_message_with_ids(
            session_id,
            600 + scope_id,
            "hook",
            "tool/editing-stop",
            "catenary",
            Some(&format!("ipc-{scope_id}")),
        )
    }

    /// IPC response — second hook message that closes the IPC scope.
    fn ipc_response(session_id: &str, scope_id: i64) -> SessionMessage {
        make_message_with_ids(
            session_id,
            700 + scope_id,
            "hook",
            "tool/editing-stop",
            "catenary",
            Some(&format!("ipc-{scope_id}")),
        )
    }

    /// LSP child sharing the IPC scope's `parent_id`.
    fn ipc_lsp_child(session_id: &str, scope_id: i64, method: &str) -> SessionMessage {
        make_message_with_ids(
            session_id,
            800 + scope_id,
            "lsp",
            method,
            "rust-analyzer",
            Some(&format!("ipc-{scope_id}")),
        )
    }

    #[test]
    fn test_ipc_done_editing_scope_with_lsp_children() {
        // IPC request opens the scope, LSP children accumulate under
        // the shared parent_id, IPC response closes it.
        let messages = vec![
            ipc_request("s1", 1),
            ipc_lsp_child("s1", 1, "textDocument/didOpen"),
            ipc_lsp_child("s1", 1, "textDocument/didSave"),
            ipc_lsp_child("s1", 1, "textDocument/diagnostic"),
            ipc_lsp_child("s1", 1, "textDocument/didClose"),
            ipc_response("s1", 1),
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 1, "all messages should form one scope");
        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected Scope entry");
        };
        assert_eq!(scope.state, ScopeState::Closed);
        assert_eq!(scope.request.r#type, "hook");
        assert_eq!(scope.request.method, "tool/editing-stop");
        assert!(scope.response.is_some());
        assert_eq!(scope.children.len(), 4, "4 LSP children");
        // Closed scope: collapsed to header only.
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_ipc_done_editing_open_scope_streams_children() {
        // IPC request + LSP children, no response yet — scope is open
        // and children are visible.
        let messages = vec![
            ipc_request("s1", 1),
            ipc_lsp_child("s1", 1, "textDocument/didOpen"),
            ipc_lsp_child("s1", 1, "textDocument/didSave"),
        ];
        let state = StreamState::new(messages);

        assert_eq!(state.entries.len(), 1);
        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected Scope entry");
        };
        assert_eq!(scope.state, ScopeState::Open);
        assert_eq!(scope.children.len(), 2);
        // Open scope: header + 2 children = 3 rows.
        assert_eq!(state.display_rows.len(), 3);
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

    // ── Paging tests ────────────────────────────────────────────────

    #[test]
    fn test_prepend_page_adds_entries_at_front() {
        // Start with scope 2.
        let mut state = StreamState::new(vec![mcp_request("s1", 2, "grep"), mcp_response("s1", 2)]);
        assert_eq!(state.entries.len(), 1);

        // Prepend scope 1 (older).
        state.prepend_page(vec![mcp_request("s1", 1, "glob"), mcp_response("s1", 1)]);

        assert_eq!(state.entries.len(), 2, "should have 2 scopes after prepend");
        // First entry should be the older scope.
        let StreamEntry::Scope(first) = &state.entries[0] else {
            panic!("expected scope");
        };
        assert_eq!(first.scope_id, "scope-1", "older scope should be first");
    }

    #[test]
    fn test_prepend_empty_marks_reached_beginning() {
        let mut state = StreamState::new(vec![make_message("s1", "hover")]);
        assert!(!state.reached_beginning);

        state.prepend_page(vec![]);
        assert!(state.reached_beginning);
    }

    #[test]
    fn test_prepend_preserves_cursor_position() {
        let mut state = StreamState::new(vec![
            mcp_request("s1", 2, "grep"),
            mcp_response("s1", 2),
            make_message("s1", "hover"),
        ]);
        // Cursor at row 1 (standalone "hover").
        state.cursor = 1;

        state.prepend_page(vec![mcp_request("s1", 1, "glob"), mcp_response("s1", 1)]);

        // Cursor should shift by 1 (one new entry prepended).
        assert_eq!(state.cursor, 2);
    }

    #[test]
    fn test_check_paging_near_top_requests_older() {
        let messages: Vec<_> = (0..5)
            .map(|i| make_message_with_ids("s1", i + 1, "lsp", &format!("m{i}"), "ra", None))
            .collect();
        let mut state = StreamState::new(messages);
        state.cursor = 0;

        let request = state.check_paging();
        assert!(
            matches!(request, Some(PageRequest::Older(1))),
            "expected Older(1), got {request:?}"
        );
    }

    #[test]
    fn test_check_paging_reached_beginning_no_request() {
        let messages: Vec<_> = (0..5)
            .map(|i| make_message_with_ids("s1", i + 1, "lsp", &format!("m{i}"), "ra", None))
            .collect();
        let mut state = StreamState::new(messages);
        state.reached_beginning = true;
        state.cursor = 0;

        assert!(
            state.check_paging().is_none(),
            "should not request page when already at beginning"
        );
    }

    #[test]
    fn test_scope_map_integrity_after_prepend() {
        // Scope 2 with a child.
        let mut state = StreamState::new(vec![
            mcp_request("s1", 2, "grep"),
            lsp_child("s1", 2, "workspace/symbol"),
            mcp_response("s1", 2),
        ]);

        // Prepend scope 1.
        state.prepend_page(vec![mcp_request("s1", 1, "glob"), mcp_response("s1", 1)]);

        // Append a message to scope 2 via tail — should find the scope.
        state.append(vec![lsp_child("s1", 2, "textDocument/references")]);

        // Scope 2 should now have 2 children (scope is closed, but
        // late-arriving children still accumulate).
        let StreamEntry::Scope(scope2) = &state.entries[1] else {
            panic!("expected scope at index 1");
        };
        assert_eq!(scope2.scope_id, "scope-2");
        assert_eq!(scope2.children.len(), 2);
    }

    // ── Session filter tests ─────────────────────────────────────────

    #[test]
    fn test_session_filter_none_shows_all() {
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            mcp_response("s1", 1),
            mcp_request("s2", 2, "glob"),
            mcp_response("s2", 2),
            make_message("daemon", "lifecycle"),
        ];
        let mut state = StreamState::new(messages);
        state.set_session_filter(None);
        assert_eq!(state.display_rows.len(), 3, "None filter shows all entries");
    }

    #[test]
    fn test_session_filter_shows_only_selected() {
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            mcp_response("s1", 1),
            mcp_request("s2", 2, "glob"),
            mcp_response("s2", 2),
            make_message("s1", "hover"),
        ];
        let mut state = StreamState::new(messages);

        let mut filter = HashSet::new();
        filter.insert("s1".to_string());
        state.set_session_filter(Some(filter));

        // Only s1's scope and standalone should show.
        assert_eq!(
            state.display_rows.len(),
            2,
            "filter should show only s1 entries"
        );
    }

    #[test]
    fn test_session_filter_hides_daemon_events() {
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            mcp_response("s1", 1),
            make_message("daemon", "lifecycle"),
            make_message("daemon", "gc"),
        ];
        let mut state = StreamState::new(messages);

        let mut filter = HashSet::new();
        filter.insert("s1".to_string());
        state.set_session_filter(Some(filter));

        // Daemon events should be hidden.
        assert_eq!(
            state.display_rows.len(),
            1,
            "daemon events should be hidden"
        );
    }

    #[test]
    fn test_session_filter_clamps_cursor() {
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            mcp_response("s1", 1),
            mcp_request("s2", 2, "glob"),
            mcp_response("s2", 2),
        ];
        let mut state = StreamState::new(messages);
        // Cursor on the second entry.
        state.cursor = 1;

        // Filter to s1 only — only 1 display row remains.
        let mut filter = HashSet::new();
        filter.insert("s1".to_string());
        state.set_session_filter(Some(filter));

        assert_eq!(state.cursor, 0, "cursor should clamp to valid range");
    }

    #[test]
    fn test_session_filter_clamps_scroll_position() {
        let messages: Vec<_> = (0..30)
            .map(|i| make_message("s1", &format!("method-{i}")))
            .collect();
        let mut state = StreamState::new(messages);
        state.scroll_position = 25;

        // Filter to non-existent session — empty display.
        let mut filter = HashSet::new();
        filter.insert("nonexistent".to_string());
        state.set_session_filter(Some(filter));

        assert_eq!(
            state.scroll_position, 0,
            "scroll should clamp when display rows shrink"
        );
    }

    #[test]
    fn test_session_filter_multiple_selected() {
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            mcp_response("s1", 1),
            mcp_request("s2", 2, "glob"),
            mcp_response("s2", 2),
            mcp_request("s3", 3, "grep"),
            mcp_response("s3", 3),
        ];
        let mut state = StreamState::new(messages);

        let mut filter = HashSet::new();
        filter.insert("s1".to_string());
        filter.insert("s3".to_string());
        state.set_session_filter(Some(filter));

        assert_eq!(state.display_rows.len(), 2, "should show s1 and s3");
    }

    #[test]
    fn test_session_filter_toggle_back_to_none() {
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            mcp_response("s1", 1),
            mcp_request("s2", 2, "glob"),
            mcp_response("s2", 2),
        ];
        let mut state = StreamState::new(messages);

        // Apply filter.
        let mut filter = HashSet::new();
        filter.insert("s1".to_string());
        state.set_session_filter(Some(filter));
        assert_eq!(state.display_rows.len(), 1);

        // Remove filter.
        state.set_session_filter(None);
        assert_eq!(state.display_rows.len(), 2, "removing filter restores all");
    }

    #[test]
    fn test_session_filter_with_open_scope_children() {
        // Open scope with children — filter should show header + children.
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            make_message("s2", "hover"),
        ];
        let mut state = StreamState::new(messages);

        let mut filter = HashSet::new();
        filter.insert("s1".to_string());
        state.set_session_filter(Some(filter));

        // Open scope (header + 1 child) = 2 rows; s2's standalone hidden.
        assert_eq!(state.display_rows.len(), 2);
    }

    // ── Server noise suppression ────────────────────────────────────

    #[test]
    fn server_noise_suppressed_from_stream() {
        // `$/progress` without parent_id should be dropped.
        let progress = SessionMessage {
            parent_id: None,
            ..make_message_with_ids("s1", 1, "lsp", "$/progress", "rust-analyzer", None)
        };
        // `window/logMessage` without parent_id should be dropped.
        let log_msg = SessionMessage {
            parent_id: None,
            ..make_message_with_ids("s1", 2, "lsp", "window/logMessage", "rust-analyzer", None)
        };
        // `window/showMessage` without parent_id should be dropped.
        let show_msg = SessionMessage {
            parent_id: None,
            ..make_message_with_ids("s1", 3, "lsp", "window/showMessage", "rust-analyzer", None)
        };
        // Normal standalone LSP should stay.
        let normal = make_message_with_ids(
            "s1",
            4,
            "lsp",
            "textDocument/definition",
            "rust-analyzer",
            None,
        );

        let state = StreamState::new(vec![progress, log_msg, show_msg, normal]);
        // Only the normal message survives.
        assert_eq!(state.entries.len(), 1);
        let StreamEntry::Standalone(msg) = &state.entries[0] else {
            panic!("expected standalone");
        };
        assert_eq!(msg.method, "textDocument/definition");
    }

    #[test]
    fn scoped_progress_not_suppressed() {
        // `$/progress` WITH parent_id is a child of a scope — not noise.
        let request = mcp_request("s1", 1, "grep");
        let progress = make_message_with_ids(
            "s1",
            5,
            "lsp",
            "$/progress",
            "rust-analyzer",
            Some("scope-1"),
        );

        let state = StreamState::new(vec![request, progress]);
        assert_eq!(state.entries.len(), 1);
        let StreamEntry::Scope(scope) = &state.entries[0] else {
            panic!("expected scope");
        };
        assert_eq!(scope.children.len(), 1);
        assert_eq!(scope.children[0].method, "$/progress");
    }

    // ── Server filter tests ──────────────────────────────────────────

    /// LSP child with a specific server for server filter tests.
    fn lsp_child_server(
        session_id: &str,
        scope_id: i64,
        id_offset: i64,
        server: &str,
    ) -> SessionMessage {
        make_message_with_ids(
            session_id,
            500 + scope_id * 10 + id_offset,
            "lsp",
            "textDocument/definition",
            server,
            Some(&format!("scope-{scope_id}")),
        )
    }

    #[test]
    fn test_server_filter_shows_matching_scopes() {
        // Scope 1: has rust-analyzer children.
        let req1 = mcp_request("s1", 1, "grep");
        let child1 = lsp_child_server("s1", 1, 0, "rust-analyzer");
        let resp1 = mcp_response("s1", 1);

        // Scope 2: has lua-ls children.
        let req2 = mcp_request("s1", 2, "grep");
        let child2 = lsp_child_server("s1", 2, 0, "lua-ls");
        let resp2 = mcp_response("s1", 2);

        let mut state = StreamState::new(vec![req1, child1, resp1, req2, child2, resp2]);
        assert_eq!(
            state.display_rows.len(),
            2,
            "both scopes visible unfiltered"
        );

        // Filter to rust-analyzer only.
        let mut filter = HashSet::new();
        filter.insert(("rust-analyzer".to_string(), String::new()));
        state.set_server_filter(Some(filter));

        assert_eq!(state.display_rows.len(), 1, "only rust-analyzer scope");
        let DisplayRow::ScopeHeader(idx) = state.display_rows[0] else {
            panic!("expected scope header");
        };
        let StreamEntry::Scope(scope) = &state.entries[idx] else {
            panic!("expected scope entry");
        };
        assert_eq!(scope.scope_id, "scope-1");
    }

    #[test]
    fn test_server_filter_hides_hook_only_scopes() {
        // Hook scope — no LSP children.
        let h_req = hook_request("s1", 1);
        let h_resp = hook_response("s1", 1);

        // MCP scope with LSP children.
        let m_req = mcp_request("s1", 2, "grep");
        let child = lsp_child_server("s1", 2, 0, "rust-analyzer");
        let m_resp = mcp_response("s1", 2);

        let mut state = StreamState::new(vec![h_req, h_resp, m_req, child, m_resp]);
        assert_eq!(
            state.display_rows.len(),
            2,
            "both scopes visible unfiltered"
        );

        let mut filter = HashSet::new();
        filter.insert(("rust-analyzer".to_string(), String::new()));
        state.set_server_filter(Some(filter));

        // Hook-only scope hidden, MCP scope visible.
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_server_filter_expanded_shows_all_children() {
        // Scope with both rust-analyzer and lua-ls children.
        let req = mcp_request("s1", 1, "grep");
        let child_ra = lsp_child_server("s1", 1, 0, "rust-analyzer");
        let child_lua = lsp_child_server("s1", 1, 1, "lua-ls");
        // Don't close the scope so it's auto-expanded (Open state).

        let mut state = StreamState::new(vec![req, child_ra, child_lua]);

        // Filter to rust-analyzer — scope matches.
        let mut filter = HashSet::new();
        filter.insert(("rust-analyzer".to_string(), String::new()));
        state.set_server_filter(Some(filter));

        // Scope header + both children visible (expansion shows all).
        assert_eq!(state.display_rows.len(), 3);
    }

    #[test]
    fn test_server_filter_combined_with_session_filter() {
        // Session s1, scope 1: rust-analyzer.
        let req1 = mcp_request("s1", 1, "grep");
        let child1 = lsp_child_server("s1", 1, 0, "rust-analyzer");
        let resp1 = mcp_response("s1", 1);

        // Session s2, scope 2: rust-analyzer.
        let req2 = mcp_request("s2", 2, "grep");
        let child2 = lsp_child_server("s2", 2, 0, "rust-analyzer");
        let resp2 = mcp_response("s2", 2);

        // Session s1, scope 3: lua-ls.
        let req3 = mcp_request("s1", 3, "glob");
        let child3 = lsp_child_server("s1", 3, 0, "lua-ls");
        let resp3 = mcp_response("s1", 3);

        let mut state = StreamState::new(vec![
            req1, child1, resp1, req2, child2, resp2, req3, child3, resp3,
        ]);
        assert_eq!(state.display_rows.len(), 3);

        // Session filter: s1 only.
        let mut sessions = HashSet::new();
        sessions.insert("s1".to_string());
        state.set_session_filter(Some(sessions));
        assert_eq!(state.display_rows.len(), 2, "s1 scopes only");

        // Add server filter: rust-analyzer only.
        let mut servers = HashSet::new();
        servers.insert(("rust-analyzer".to_string(), String::new()));
        state.set_server_filter(Some(servers));

        // Intersection: s1 AND rust-analyzer = scope 1 only.
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_server_filter_clear_restores_all() {
        let req = mcp_request("s1", 1, "grep");
        let child = lsp_child_server("s1", 1, 0, "rust-analyzer");
        let resp = mcp_response("s1", 1);

        let standalone = make_message("s1", "textDocument/hover");

        let mut state = StreamState::new(vec![req, child, resp, standalone]);
        assert_eq!(state.display_rows.len(), 2);

        // Apply filter.
        let mut filter = HashSet::new();
        filter.insert(("lua-ls".to_string(), String::new()));
        state.set_server_filter(Some(filter));
        assert_eq!(state.display_rows.len(), 0, "nothing matches lua-ls");

        // Clear filter.
        state.set_server_filter(None);
        assert_eq!(state.display_rows.len(), 2, "all restored");
    }

    // ── Internal message expansion tests ─────────────────────────────

    /// Standalone internal message with payload.
    fn internal_message(session_id: &str, id: i64) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            payload: serde_json::json!({
                "level": "warn",
                "message": "Failed to load workspaces",
                "source": "server.lifecycle",
                "language": "rust"
            }),
            ..test_support::message_with_ids(
                id,
                "internal",
                "catenary_mcp::lsp::manager",
                "rust-analyzer",
                None,
            )
        }
    }

    #[test]
    fn test_internal_standalone_initially_collapsed() {
        let state = StreamState::new(vec![internal_message("s1", 1)]);
        assert_eq!(state.display_rows.len(), 1);
        assert_eq!(state.display_rows[0], DisplayRow::Standalone(0));
    }

    #[test]
    fn test_internal_standalone_toggle_expands() {
        let mut state = StreamState::new(vec![internal_message("s1", 1)]);
        assert_eq!(state.display_rows.len(), 1);

        // Toggle expand.
        state.toggle_expansion();
        // target + source + language = 3 detail rows + 1 header.
        assert!(
            state.display_rows.len() > 1,
            "should expand with detail rows, got {}",
            state.display_rows.len()
        );
        assert_eq!(state.display_rows[0], DisplayRow::Standalone(0));
        assert!(
            matches!(
                state.display_rows[1],
                DisplayRow::StandaloneDetail { entry_idx: 0, .. }
            ),
            "second row should be a detail"
        );
    }

    #[test]
    fn test_internal_standalone_toggle_collapses() {
        let mut state = StreamState::new(vec![internal_message("s1", 1)]);
        state.toggle_expansion(); // expand
        let expanded_count = state.display_rows.len();
        assert!(expanded_count > 1);

        state.toggle_expansion(); // collapse
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_internal_standalone_detail_last_flag() {
        let mut state = StreamState::new(vec![internal_message("s1", 1)]);
        state.toggle_expansion();

        let last_row = state.display_rows.last().expect("should have rows");
        match last_row {
            DisplayRow::StandaloneDetail { is_last, .. } => {
                assert!(is_last, "last detail row should have is_last=true");
            }
            other => panic!("expected StandaloneDetail, got {other:?}"),
        }

        // Non-last detail rows should have is_last=false.
        if state.display_rows.len() > 2 {
            match &state.display_rows[1] {
                DisplayRow::StandaloneDetail { is_last, .. } => {
                    assert!(!is_last, "first detail row should have is_last=false");
                }
                other => panic!("expected StandaloneDetail, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_non_internal_standalone_not_expandable() {
        let msg = make_message("s1", "textDocument/hover");
        let mut state = StreamState::new(vec![msg]);
        assert_eq!(state.display_rows.len(), 1);

        state.toggle_expansion();
        // LSP standalone should not expand.
        assert_eq!(state.display_rows.len(), 1);
    }

    #[test]
    fn test_internal_expansion_survives_prepend() {
        let mut state = StreamState::new(vec![internal_message("s1", 10)]);
        state.toggle_expansion();
        let expanded_before = state.display_rows.len();
        assert!(expanded_before > 1, "should be expanded");

        // Prepend a page of older messages.
        let older = vec![mcp_request("s1", 1, "grep"), mcp_response("s1", 1)];
        state.prepend_page(older);

        // The internal message moved from index 0 to index 1.
        // It should still be expanded.
        let detail_count = state
            .display_rows
            .iter()
            .filter(|r| matches!(r, DisplayRow::StandaloneDetail { .. }))
            .count();
        assert!(
            detail_count > 0,
            "expansion should survive prepend, detail rows: {detail_count}"
        );
    }

    #[test]
    fn test_internal_yank_detail_row() {
        let icons = super::super::icons::IconSet::from_config(crate::config::IconConfig::default());
        let mut state = StreamState::new(vec![internal_message("s1", 1)]);
        state.toggle_expansion();

        // Move cursor to the first detail row.
        state.cursor = 1;
        let text = state.yank_text(&icons).expect("should yank detail");
        assert!(
            text.contains(": "),
            "detail yank should contain label: value, got: {text}"
        );
    }

    // ── Search tests ───────────────────────────────────────────────

    fn icons() -> super::super::icons::IconSet {
        super::super::icons::IconSet::from_config(crate::config::IconConfig::default())
    }

    #[test]
    fn search_finds_matching_rows() {
        let icons = icons();
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "textDocument/completion"),
            make_message("s1", "workspace/symbol"),
        ];
        let mut state = StreamState::new(messages);
        state.set_search(Some("hover".to_string()), &icons);

        assert_eq!(state.search_matches.len(), 1);
        assert_eq!(state.search_match_idx, Some(0));
        // Cursor should jump to the match.
        assert_eq!(state.cursor, state.search_matches[0]);
    }

    #[test]
    fn search_case_insensitive() {
        let icons = icons();
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "workspace/symbol"),
        ];
        let mut state = StreamState::new(messages);
        state.set_search(Some("HOVER".to_string()), &icons);

        assert_eq!(state.search_matches.len(), 1);
    }

    #[test]
    fn search_no_matches() {
        let icons = icons();
        let messages = vec![make_message("s1", "textDocument/hover")];
        let mut state = StreamState::new(messages);
        state.set_search(Some("nonexistent".to_string()), &icons);

        assert!(state.search_matches.is_empty());
        assert_eq!(state.search_match_idx, None);
        assert_eq!(state.search_status().as_deref(), Some("no matches"));
    }

    #[test]
    fn search_next_wraps_around() {
        let icons = icons();
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "workspace/symbol"),
            make_message("s1", "textDocument/hover"),
        ];
        let mut state = StreamState::new(messages);
        // All three are standalones containing "rust-analyzer".
        state.set_search(Some("rust-analyzer".to_string()), &icons);
        assert_eq!(state.search_matches.len(), 3);

        let first = state.cursor;
        state.search_next(40);
        let second = state.cursor;
        assert!(second > first);

        state.search_next(40);
        let third = state.cursor;
        assert!(third > second);

        // Wrap around.
        state.search_next(40);
        assert_eq!(state.cursor, first);
    }

    #[test]
    fn search_prev_wraps_around() {
        let icons = icons();
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "workspace/symbol"),
            make_message("s1", "textDocument/completion"),
        ];
        let mut state = StreamState::new(messages);
        state.set_search(Some("rust-analyzer".to_string()), &icons);
        assert_eq!(state.search_matches.len(), 3);

        let first = state.cursor;
        // Go to the last match.
        state.search_prev(40);
        assert!(state.cursor > first, "prev should wrap to last match");
    }

    #[test]
    fn clear_search_removes_state() {
        let icons = icons();
        let messages = vec![make_message("s1", "textDocument/hover")];
        let mut state = StreamState::new(messages);
        state.set_search(Some("hover".to_string()), &icons);
        assert!(state.has_search());

        state.clear_search();
        assert!(!state.has_search());
        assert!(state.search_matches.is_empty());
        assert_eq!(state.search_match_idx, None);
        assert!(state.search_status().is_none());
    }

    #[test]
    fn search_empty_query_clears() {
        let icons = icons();
        let messages = vec![make_message("s1", "textDocument/hover")];
        let mut state = StreamState::new(messages);
        state.set_search(Some("hover".to_string()), &icons);
        assert!(state.has_search());

        state.set_search(Some(String::new()), &icons);
        assert!(!state.has_search());
    }

    #[test]
    fn search_status_format() {
        let icons = icons();
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "workspace/symbol"),
            make_message("s1", "textDocument/hover"),
        ];
        let mut state = StreamState::new(messages);
        state.set_search(Some("hover".to_string()), &icons);

        let status = state.search_status().expect("should have status");
        assert_eq!(status, "1/2");

        state.search_next(40);
        let status = state.search_status().expect("should have status");
        assert_eq!(status, "2/2");
    }

    #[test]
    fn is_search_match_returns_correct_rows() {
        let icons = icons();
        let messages = vec![
            make_message("s1", "textDocument/hover"),
            make_message("s1", "workspace/symbol"),
        ];
        let mut state = StreamState::new(messages);
        state.set_search(Some("hover".to_string()), &icons);

        assert!(state.is_search_match(0));
        assert!(!state.is_search_match(1));
    }

    #[test]
    fn search_next_scrolls_viewport() {
        let icons = icons();
        // Create enough messages that matches span multiple viewports.
        let mut messages = Vec::new();
        for i in 0..60 {
            let method = if i % 20 == 0 {
                "textDocument/hover"
            } else {
                "workspace/symbol"
            };
            messages.push(SessionMessage {
                id: i64::from(i),
                ..make_message("s1", method)
            });
        }
        let mut state = StreamState::new(messages);
        let viewport = 10;
        state.set_search(Some("hover".to_string()), &icons);

        // First match at row 0.
        assert_eq!(state.cursor, 0);

        // Next match should be row 20 — viewport should scroll.
        state.search_next(viewport);
        assert_eq!(state.cursor, 20);
        assert!(
            state.scroll_position <= 20 && state.cursor < state.scroll_position + viewport,
            "cursor should be visible: scroll={}, cursor={}, viewport={viewport}",
            state.scroll_position,
            state.cursor
        );
    }

    #[test]
    fn search_recomputes_after_append() {
        let icons = icons();
        let messages = vec![make_message("s1", "textDocument/hover")];
        let mut state = StreamState::new(messages);
        state.set_search(Some("symbol".to_string()), &icons);
        assert_eq!(state.search_matches.len(), 0, "no matches yet");

        // Append a message that matches.
        state.append(vec![make_message("s1", "workspace/symbol")]);
        // Matches are stale until recompute.
        state.recompute_search_if_dirty(&icons);
        assert_eq!(state.search_matches.len(), 1, "new message should be found");
    }

    #[test]
    fn search_recomputes_after_prepend() {
        let icons = icons();
        let messages = vec![SessionMessage {
            id: 10,
            ..make_message("s1", "workspace/symbol")
        }];
        let mut state = StreamState::new(messages);
        state.set_search(Some("hover".to_string()), &icons);
        assert_eq!(state.search_matches.len(), 0);

        // Prepend an older message that matches.
        let older = vec![SessionMessage {
            id: 1,
            ..make_message("s1", "textDocument/hover")
        }];
        state.prepend_page(older);
        state.recompute_search_if_dirty(&icons);
        assert_eq!(
            state.search_matches.len(),
            1,
            "prepended message should be found"
        );
    }

    #[test]
    fn search_dirty_flag_not_set_without_query() {
        let messages = vec![make_message("s1", "textDocument/hover")];
        let mut state = StreamState::new(messages);
        // No search active — append should not set dirty.
        state.append(vec![make_message("s1", "workspace/symbol")]);
        assert!(!state.search_dirty, "no query means no dirty flag");
    }

    // ── Visual selection tests ──────────────────────────────────────

    #[test]
    fn test_visual_mode_lifecycle() {
        let state = StreamState::new(vec![
            make_message("s1", "initialize"),
            make_message("s1", "shutdown"),
        ]);
        assert!(!state.in_visual());
        assert!(state.visual_range().is_none());
    }

    #[test]
    fn test_visual_start_sets_anchor() {
        let mut state = StreamState::new(vec![
            make_message("s1", "initialize"),
            make_message("s1", "shutdown"),
        ]);
        state.cursor = 0;
        state.start_visual();
        assert!(state.in_visual());
        assert_eq!(state.visual_range(), Some((0, 0)));
    }

    #[test]
    fn test_visual_range_expands_with_cursor() {
        let mut state = StreamState::new(vec![
            make_message("s1", "a"),
            make_message("s1", "b"),
            make_message("s1", "c"),
        ]);
        state.cursor = 0;
        state.start_visual();
        state.cursor_down(2, 100);
        assert_eq!(state.visual_range(), Some((0, 2)));
    }

    #[test]
    fn test_visual_range_cursor_above_anchor() {
        let mut state = StreamState::new(vec![
            make_message("s1", "a"),
            make_message("s1", "b"),
            make_message("s1", "c"),
        ]);
        state.cursor = 2;
        state.start_visual();
        state.cursor_up(2);
        assert_eq!(state.visual_range(), Some((0, 2)));
    }

    #[test]
    fn test_visual_exit_clears_anchor() {
        let mut state = StreamState::new(vec![make_message("s1", "a")]);
        state.start_visual();
        assert!(state.in_visual());
        state.exit_visual();
        assert!(!state.in_visual());
        assert!(state.visual_range().is_none());
    }

    #[test]
    fn test_visual_yank_multiple_rows() {
        let icons = icons();
        let mut state = StreamState::new(vec![
            make_message("s1", "initialize"),
            make_message("s1", "shutdown"),
            make_message("s1", "exit"),
        ]);
        state.cursor = 0;
        state.start_visual();
        state.cursor_down(2, 100);

        let text = state.yank_text(&icons).expect("should yank visual range");
        assert_eq!(text.lines().count(), 3, "should yank 3 lines, got: {text}");
    }

    #[test]
    fn test_visual_yank_single_row_without_visual() {
        let icons = icons();
        let mut state = StreamState::new(vec![
            make_message("s1", "initialize"),
            make_message("s1", "shutdown"),
        ]);
        state.cursor = 0;

        let text = state.yank_text(&icons).expect("should yank single row");
        assert_eq!(
            text.lines().count(),
            1,
            "without visual: single line, got: {text}"
        );
    }

    #[test]
    fn test_visual_yank_expanded_scope_children() {
        let icons = icons();
        let messages = vec![
            mcp_request("s1", 1, "grep"),
            lsp_child("s1", 1, "workspace/symbol"),
            lsp_child("s1", 1, "textDocument/references"),
            mcp_response("s1", 1),
        ];
        let mut state = StreamState::new(messages);
        state.toggle_expansion();
        assert_eq!(state.display_rows.len(), 3, "header + 2 children");

        state.cursor = 0;
        state.start_visual();
        state.cursor_down(2, 100);

        let text = state.yank_text(&icons).expect("should yank scope rows");
        assert_eq!(
            text.lines().count(),
            3,
            "should yank header + 2 children, got: {text}"
        );
    }

    #[test]
    fn test_visual_suppresses_paging() {
        let mut state = StreamState::new(vec![make_message("s1", "a"), make_message("s1", "b")]);
        state.reached_beginning = false;
        state.cursor = 0;

        assert!(state.check_paging().is_some());

        state.start_visual();
        assert!(state.check_paging().is_none());
    }

    #[test]
    fn test_visual_suppresses_auto_scroll() {
        let mut state = StreamState::new(vec![
            make_message("s1", "a"),
            make_message("s1", "b"),
            make_message("s1", "c"),
        ]);
        state.auto_scroll = true;
        state.start_visual();

        state.cursor = 0;
        state.scroll_position = 0;
        state.apply_auto_scroll(2);
        assert_eq!(state.cursor, 0, "cursor should not move during visual");
        assert_eq!(
            state.scroll_position, 0,
            "scroll should not move during visual"
        );
    }

    #[test]
    fn test_visual_cursor_down_does_not_reenable_auto_scroll() {
        let mut state = StreamState::new(vec![make_message("s1", "a"), make_message("s1", "b")]);
        state.auto_scroll = false;
        state.cursor = 0;
        state.start_visual();

        state.cursor_down(1, 100);
        assert!(
            !state.auto_scroll,
            "auto_scroll should stay off during visual"
        );
    }
}
