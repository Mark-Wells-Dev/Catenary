// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Sidebar widget: session filter list and server dashboard.
//!
//! Built in tickets 03–07. This module provides the session list
//! sidebar with hex badges, host format labels, and primary root
//! names. Sessions appear on connect and disappear on disconnect.

use std::collections::HashSet;
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::app::FocusRegion;
use super::data::{ServerNoiseRow, ServerStatusRow};
use super::stream::HexBadgeMap;
use super::theme::Theme;

/// Minimum sidebar width (chars, including separator column).
const MIN_WIDTH: u16 = 20;
/// Maximum sidebar width (chars, including separator column).
const MAX_WIDTH: u16 = 30;

// ── Session entry ────────────────────────────────────────────────────

/// A session entry in the sidebar.
pub struct SessionEntry {
    /// Session ID (database key).
    pub session_id: String,
    /// Two-character hex badge.
    pub badge: String,
    /// Host format label ("claude", "gemini", etc.).
    pub host: String,
    /// Primary workspace root directory name with trailing slash.
    pub root: String,
}

impl SessionEntry {
    /// Display width of this entry: `XX host Root/`.
    const fn display_width(&self) -> usize {
        // badge(2) + space(1) + host + space(1) + root
        2 + 1 + self.host.len() + 1 + self.root.len()
    }
}

// ── Server entry ────────────────────────────────────────────────────

/// A per-instance server entry in the sidebar.
///
/// Each entry represents one server process serving one workspace root.
/// Every root gets its own server instance (misc 84 — per-root isolation).
/// Header shows: `name (root/)  state`. Child lines show active progress
/// and the most recent server message.
pub struct ServerEntry {
    /// Server binary name (config key, e.g., "rust-analyzer").
    pub name: String,
    /// Full scope root path (e.g., "/home/user/project").
    ///
    /// Used as part of the `(name, scope_root)` selection key.
    pub scope_root: String,
    /// Workspace root short name for display (e.g., "Catenary/").
    pub root: String,
    /// Lifecycle display state (`"initializing"`, `"ready"`, `"busy"`, `"dead"`).
    pub state: String,
    /// Active progress line (e.g., `"Indexing… 47%"`). `None` when idle.
    pub progress_line: Option<String>,
    /// Most recent `window/logMessage` or `window/showMessage` content.
    pub server_message: Option<String>,
}

impl ServerEntry {
    /// Display width of the header line: `name (root)  state`.
    const fn header_width(&self) -> usize {
        if self.root.is_empty() {
            // name + 2 spaces + state
            self.name.len() + 2 + self.state.len()
        } else {
            // name + space + ( + root + ) + 2 spaces + state
            self.name.len() + 1 + 1 + self.root.len() + 1 + 2 + self.state.len()
        }
    }

    /// Number of child lines rendered under this entry.
    #[must_use]
    pub const fn child_count(&self) -> usize {
        let p = if self.progress_line.is_some() { 1 } else { 0 };
        let m = if self.server_message.is_some() { 1 } else { 0 };
        p + m
    }
}

// ── Sidebar state ────────────────────────────────────────────────────

/// Server instance identity: `(server_name, scope_root)`.
///
/// Used as the selection key so two instances of the same server binary
/// serving different workspace roots can be filtered independently.
pub type ServerInstanceKey = (String, String);

/// Sidebar state: session list, server list, per-section cursors, and
/// selection filters.
pub struct SidebarState {
    /// Alive sessions in display order.
    pub entries: Vec<SessionEntry>,
    /// Active server entries, grouped by server name.
    pub servers: Vec<ServerEntry>,
    /// Cursor position in the session list.
    pub cursor: usize,
    /// First visible session entry index (scroll offset).
    pub scroll_offset: usize,
    /// Cursor position in the server list.
    pub server_cursor: usize,
    /// First visible server entry index (scroll offset).
    pub server_scroll_offset: usize,
    /// Session IDs from the last refresh (for change detection).
    last_ids: Vec<String>,
    /// Server names from the last refresh (for change detection).
    last_server_names: Vec<String>,
    /// Selected session IDs (for stream filtering).
    /// Empty = show all (no filter active).
    selected: HashSet<String>,
    /// Selected server instances `(name, scope_root)` (for stream filtering).
    /// Empty = show all (no server filter active).
    selected_servers: HashSet<ServerInstanceKey>,
}

impl SidebarState {
    /// Create an empty sidebar.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            servers: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            server_cursor: 0,
            server_scroll_offset: 0,
            last_ids: Vec::new(),
            last_server_names: Vec::new(),
            selected: HashSet::new(),
            selected_servers: HashSet::new(),
        }
    }

    /// Check whether the alive session IDs have changed since the last
    /// refresh.
    #[must_use]
    pub fn needs_refresh(&self, current_ids: &[String]) -> bool {
        self.last_ids != current_ids
    }

    /// Update the sidebar with fresh session data.
    ///
    /// Accepts tuples of `(session_id, client_name, workspace)` for
    /// alive sessions only. Releases badges for sessions that have
    /// disconnected and assigns badges for new ones.
    pub fn refresh(
        &mut self,
        sessions: Vec<(String, Option<String>, String)>,
        badges: &mut HexBadgeMap,
    ) {
        // Release badges for removed sessions and prune stale selections.
        let new_ids: HashSet<&str> = sessions.iter().map(|(id, _, _)| id.as_str()).collect();
        for entry in &self.entries {
            if !new_ids.contains(entry.session_id.as_str()) {
                badges.release(&entry.session_id);
            }
        }
        self.selected.retain(|id| new_ids.contains(id.as_str()));
        drop(new_ids);

        self.entries = sessions
            .into_iter()
            .map(|(id, client_name, workspace)| {
                let badge = badges.badge(&id);
                let host = host_label(client_name.as_deref());
                let root = root_name(&workspace);
                SessionEntry {
                    session_id: id,
                    badge,
                    host,
                    root,
                }
            })
            .collect();

        self.last_ids = self.entries.iter().map(|e| e.session_id.clone()).collect();

        // Clamp cursor to session count.
        let max = self.entries.len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
    }

    /// Toggle selection on the entry at the current cursor position.
    ///
    /// Returns `true` if the selection set changed (caller should update
    /// the stream filter).
    pub fn toggle_selected(&mut self) -> bool {
        let Some(entry) = self.entries.get(self.cursor) else {
            return false;
        };
        let id = &entry.session_id;
        if self.selected.contains(id) {
            self.selected.remove(id);
        } else {
            self.selected.insert(id.clone());
        }
        true
    }

    /// Return the active session filter.
    ///
    /// `None` = show all (no sessions selected). `Some(set)` = show only
    /// scopes belonging to sessions in the set.
    #[must_use]
    pub fn session_filter(&self) -> Option<HashSet<String>> {
        if self.selected.is_empty() {
            None
        } else {
            Some(self.selected.clone())
        }
    }

    /// Whether any session filter is active.
    #[must_use]
    pub fn has_filter(&self) -> bool {
        !self.selected.is_empty()
    }

    /// Whether a specific session is selected.
    #[must_use]
    pub fn is_selected(&self, session_id: &str) -> bool {
        self.selected.contains(session_id)
    }

    // ── Server selection ─────────────────────────────────────────────

    /// Toggle selection on the server at the current server cursor.
    ///
    /// Toggles by `(name, scope_root)` pair: each server instance can
    /// be independently selected/deselected. Returns `true` if the
    /// selection set changed.
    pub fn toggle_server_selected(&mut self) -> bool {
        let Some(entry) = self.servers.get(self.server_cursor) else {
            return false;
        };
        let key: ServerInstanceKey = (entry.name.clone(), entry.scope_root.clone());
        if self.selected_servers.contains(&key) {
            self.selected_servers.remove(&key);
        } else {
            self.selected_servers.insert(key);
        }
        true
    }

    /// Return the active server filter.
    ///
    /// `None` = show all (no servers selected). `Some(set)` = show only
    /// scopes involving server instances in the set.
    #[must_use]
    pub fn server_filter(&self) -> Option<HashSet<ServerInstanceKey>> {
        if self.selected_servers.is_empty() {
            None
        } else {
            Some(self.selected_servers.clone())
        }
    }

    /// Whether any server filter is active.
    #[must_use]
    pub fn has_server_filter(&self) -> bool {
        !self.selected_servers.is_empty()
    }

    /// Whether a specific server instance is selected.
    #[must_use]
    pub fn is_server_selected(&self, name: &str, scope_root: &str) -> bool {
        self.selected_servers
            .contains(&(name.to_string(), scope_root.to_string()))
    }

    // ── Server list refresh ─────────────────────────────────────────

    /// Check whether the server list has changed since the last refresh.
    #[must_use]
    pub fn servers_need_refresh(&self, rows: &[ServerStatusRow], noise: &[ServerNoiseRow]) -> bool {
        let mut current: Vec<String> = rows
            .iter()
            .map(|r| format!("{}:{}:{}", r.server, r.scope_root, r.state))
            .collect();
        for n in noise {
            current.push(format!("noise:{}:{}:{}", n.server, n.method, n.payload));
        }
        self.last_server_names != current
    }

    /// Update the server list from DB rows and noise data.
    ///
    /// Each status row becomes one sidebar entry — one entry per server
    /// process. Noise rows populate progress and server message children.
    /// Prunes stale server selections for servers that no longer exist.
    pub fn refresh_servers(&mut self, rows: &[ServerStatusRow], noise: &[ServerNoiseRow]) {
        self.last_server_names = rows
            .iter()
            .map(|r| format!("{}:{}:{}", r.server, r.scope_root, r.state))
            .collect();
        for n in noise {
            self.last_server_names
                .push(format!("noise:{}:{}:{}", n.server, n.method, n.payload));
        }

        self.servers = rows
            .iter()
            .map(|row| {
                let progress_line = extract_progress_line(noise, &row.server);
                let server_message = extract_server_message(noise, &row.server);
                ServerEntry {
                    name: row.server.clone(),
                    scope_root: row.scope_root.clone(),
                    root: server_root_name(&row.scope_root),
                    state: row.state.clone(),
                    progress_line,
                    server_message,
                }
            })
            .collect();

        // Prune stale server selections.
        let live_keys: HashSet<(&str, &str)> = self
            .servers
            .iter()
            .map(|s| (s.name.as_str(), s.scope_root.as_str()))
            .collect();
        self.selected_servers
            .retain(|k| live_keys.contains(&(k.0.as_str(), k.1.as_str())));

        // Clamp server cursor.
        let max = self.servers.len().saturating_sub(1);
        self.server_cursor = self.server_cursor.min(max);
    }

    // ── Session cursor navigation ──────────────────────────────────

    /// Move session cursor up by `n` entries, scrolling if needed.
    ///
    /// `visible` is the number of entry rows visible in the sidebar
    /// (total height minus the header row).
    pub const fn cursor_up(&mut self, n: usize, visible: usize) {
        let _ = visible; // used only for scroll_offset adjustment
        self.cursor = self.cursor.saturating_sub(n);
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        }
    }

    /// Move session cursor down by `n` entries, scrolling if needed.
    ///
    /// `visible` is the number of entry rows visible in the sidebar.
    pub fn cursor_down(&mut self, n: usize, visible: usize) {
        let total = self.entries.len();
        if total == 0 {
            return;
        }
        let max = total.saturating_sub(1);
        self.cursor = (self.cursor + n).min(max);
        // Scroll down to keep cursor in view.
        if visible > 0 && self.cursor >= self.scroll_offset + visible {
            self.scroll_offset = self.cursor + 1 - visible;
        }
    }

    // ── Server cursor navigation ───────────────────────────────────

    /// Move server cursor up by `n` entries, scrolling if needed.
    pub const fn server_cursor_up(&mut self, n: usize, visible: usize) {
        let _ = visible;
        self.server_cursor = self.server_cursor.saturating_sub(n);
        if self.server_cursor < self.server_scroll_offset {
            self.server_scroll_offset = self.server_cursor;
        }
    }

    /// Move server cursor down by `n` entries, scrolling if needed.
    pub fn server_cursor_down(&mut self, n: usize, visible: usize) {
        let total = self.servers.len();
        if total == 0 {
            return;
        }
        let max = total.saturating_sub(1);
        self.server_cursor = (self.server_cursor + n).min(max);
        if visible > 0 && self.server_cursor >= self.server_scroll_offset + visible {
            self.server_scroll_offset = self.server_cursor + 1 - visible;
        }
    }

    /// Compute the total sidebar width including the separator column.
    ///
    /// Auto-sizes based on the longest session or server label, clamped
    /// between [`MIN_WIDTH`] and [`MAX_WIDTH`].
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "entry widths are always small (< MAX_WIDTH)"
    )]
    pub fn content_width(&self) -> u16 {
        let max_session = self
            .entries
            .iter()
            .map(SessionEntry::display_width)
            .max()
            .unwrap_or(0);
        let max_server = self
            .servers
            .iter()
            .map(ServerEntry::header_width)
            .max()
            .unwrap_or(0);
        // "Sessions" and "Servers" headers are 8/7 chars.
        let width = max_session.max(max_server).max(8) as u16;
        // +1 for the vertical separator column.
        (width + 1).clamp(MIN_WIDTH, MAX_WIDTH)
    }

    /// Total number of visible rows in the sidebar content area.
    ///
    /// Sessions header + session entries + blank line + servers header +
    /// server entries (each with optional child lines).
    #[must_use]
    pub fn total_rows(&self) -> usize {
        let session_rows = 1 + self.entries.len(); // header + entries
        let server_header = if self.servers.is_empty() { 0 } else { 2 }; // blank + header
        let server_rows: usize = self.servers.iter().map(|s| 1 + s.child_count()).sum();
        session_rows + server_header + server_rows
    }
}

impl Default for SidebarState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Label helpers ────────────────────────────────────────────────────

/// Derive host label from `client_name`.
///
/// Maps known host CLI names to short labels. Falls back to the raw
/// name or `"agent"` when absent.
fn host_label(client_name: Option<&str>) -> String {
    match client_name {
        Some(name) if name.contains("claude") => "claude".to_string(),
        Some(name) if name.contains("gemini") => "gemini".to_string(),
        Some(name) => name.to_string(),
        None => "agent".to_string(),
    }
}

/// Extract the primary root directory name from a workspace string.
///
/// Takes the first comma-separated component and extracts the last
/// path segment, appending a trailing slash.
fn root_name(workspace: &str) -> String {
    let primary = workspace.split(',').next().unwrap_or(workspace).trim();
    let name = Path::new(primary)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(primary);
    format!("{name}/")
}

/// Extract the last path segment from a scope root path.
///
/// Returns an empty string if the path is empty.
fn server_root_name(scope_root: &str) -> String {
    if scope_root.is_empty() {
        return String::new();
    }
    let name = Path::new(scope_root)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(scope_root);
    format!("{name}/")
}

// ── Noise extraction ────────────────────────────────────────────────

/// Extract a progress line from noise data for the given server.
///
/// Reads the `$/progress` payload to build a display string like
/// `"Indexing… 47%"`. Returns `None` if no active progress or if the
/// most recent progress was an `end` event.
fn extract_progress_line(noise: &[ServerNoiseRow], server: &str) -> Option<String> {
    let row = noise
        .iter()
        .find(|n| n.server == server && n.method == "$/progress")?;

    let value = row.payload.get("params").and_then(|p| p.get("value"))?;
    let kind = value.get("kind").and_then(|k| k.as_str());

    // End events mean no active progress.
    if kind == Some("end") {
        return None;
    }

    let title = value.get("title").and_then(|t| t.as_str());
    let message = value.get("message").and_then(|m| m.as_str());
    let pct = value.get("percentage").and_then(serde_json::Value::as_u64);

    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = title {
        parts.push(format!("{t}…"));
    } else if let Some(m) = message {
        parts.push(format!("{m}…"));
    }
    if let Some(p) = pct {
        parts.push(format!("{p}%"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Extract the most recent server message from noise data.
///
/// Reads `window/logMessage` or `window/showMessage` payload to get the
/// message text. Prefers `showMessage` over `logMessage` if both exist.
fn extract_server_message(noise: &[ServerNoiseRow], server: &str) -> Option<String> {
    // Prefer showMessage (user-facing) over logMessage (telemetry).
    let row = noise
        .iter()
        .find(|n| n.server == server && n.method == "window/showMessage")
        .or_else(|| {
            noise
                .iter()
                .find(|n| n.server == server && n.method == "window/logMessage")
        })?;

    row.payload
        .get("params")
        .and_then(|p| p.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

// ── Hit map ─────────────────────────────────────────────────────────

/// What a sidebar row maps to for mouse click handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarHit {
    /// A session entry at the given index.
    Session(usize),
    /// A server entry header at the given index.
    Server(usize),
}

/// Row-to-entry mapping built during rendering.
///
/// Returned by [`render_sidebar`] so the click handler stays in sync
/// with the rendered layout automatically.
#[derive(Debug, Default)]
pub struct SidebarHitMap {
    /// `(terminal_row, hit)` pairs, in ascending row order.
    hits: Vec<(u16, SidebarHit)>,
}

impl SidebarHitMap {
    /// Create an empty hit map.
    #[must_use]
    pub const fn new() -> Self {
        Self { hits: Vec::new() }
    }

    /// Look up what occupies the given terminal row.
    #[must_use]
    pub fn hit_test(&self, row: u16) -> Option<&SidebarHit> {
        self.hits
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, hit)| hit)
    }
}

// ── Rendering ────────────────────────────────────────────────────────

/// Render the sidebar into the given area.
///
/// Returns a [`SidebarHitMap`] mapping terminal rows to clickable entries.
///
/// The rightmost column of `area` renders a vertical separator (`│`).
/// The rest shows two sections: "Sessions" (top) and "Servers" (bottom),
/// separated by a blank line. The `focus` parameter determines which
/// section shows a cursor highlight and bright header.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "terminal coordinates are always small; server child lines add necessary rendering logic"
)]
pub fn render_sidebar(
    state: &SidebarState,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    focus: FocusRegion,
) -> SidebarHitMap {
    let mut hit_map = SidebarHitMap::new();

    if area.width < 3 || area.height == 0 {
        return hit_map;
    }

    // Reserve rightmost column for the vertical separator.
    let content_width = area.width.saturating_sub(1);
    let sessions_focused = focus == FocusRegion::Sessions;
    let servers_focused = focus == FocusRegion::Servers;
    let sidebar_focused = sessions_focused || servers_focused;

    let max_rows = area.height as usize;
    let mut row: usize = 0;

    // ── Sessions header ─────────────────────────────────────────────
    let session_header_style = if sessions_focused {
        theme.title
    } else {
        theme.muted
    };
    let header = Line::from(Span::styled("Sessions", session_header_style));
    buf.set_line(area.x, area.y + row as u16, &header, content_width);
    row += 1;

    // ── Session entries ─────────────────────────────────────────────
    let has_filter = state.has_filter();
    for (i, entry) in state.entries.iter().enumerate() {
        if row >= max_rows {
            break;
        }

        let is_cursor = sessions_focused && i == state.cursor;

        // Bright when selected (or when no filter is active); dim otherwise.
        let is_bright = !has_filter || state.is_selected(&entry.session_id);
        let badge_style = if is_bright { theme.accent } else { theme.muted };
        let host_style = theme.muted;
        let text_style = if is_bright { theme.text } else { theme.muted };

        let line = Line::from(vec![
            Span::styled(&entry.badge, badge_style),
            Span::raw(" "),
            Span::styled(&entry.host, host_style),
            Span::raw(" "),
            Span::styled(&entry.root, text_style),
        ]);

        let y = area.y + row as u16;
        buf.set_line(area.x, y, &line, content_width);
        hit_map.hits.push((y, SidebarHit::Session(i)));

        // Highlight cursor row.
        if is_cursor {
            for x in area.x..area.x + content_width {
                let cell = &mut buf[(x, y)];
                if let Some(bg) = theme.selection.bg {
                    cell.set_bg(bg);
                } else {
                    cell.modifier |= ratatui::style::Modifier::REVERSED;
                }
            }
        }

        row += 1;
    }

    // ── Servers section ─────────────────────────────────────────────
    if !state.servers.is_empty() && row < max_rows {
        // Blank separator line.
        row += 1;

        if row < max_rows {
            let server_header_style = if servers_focused {
                theme.title
            } else {
                theme.muted
            };
            let server_header = Line::from(Span::styled("Servers", server_header_style));
            buf.set_line(area.x, area.y + row as u16, &server_header, content_width);
            row += 1;
        }

        let has_server_filter = state.has_server_filter();
        for (si, server) in state.servers.iter().enumerate() {
            if row >= max_rows {
                break;
            }

            let is_cursor = servers_focused && si == state.server_cursor;

            // Bright when selected (or when no server filter is active); dim otherwise.
            let is_bright =
                !has_server_filter || state.is_server_selected(&server.name, &server.scope_root);
            let name_style = if is_bright { theme.text } else { theme.muted };
            let state_style = if is_bright {
                lifecycle_style(theme, &server.state)
            } else {
                theme.muted
            };

            // Server line: "name (root/)  state" or "name  state"
            let line = if server.root.is_empty() {
                Line::from(vec![
                    Span::styled(&server.name, name_style),
                    Span::raw("  "),
                    Span::styled(&server.state, state_style),
                ])
            } else {
                Line::from(vec![
                    Span::styled(&server.name, name_style),
                    Span::raw(" "),
                    Span::styled("(", theme.muted),
                    Span::styled(&server.root, theme.muted),
                    Span::styled(")", theme.muted),
                    Span::raw("  "),
                    Span::styled(&server.state, state_style),
                ])
            };
            let y = area.y + row as u16;
            buf.set_line(area.x, y, &line, content_width);
            hit_map.hits.push((y, SidebarHit::Server(si)));

            if is_cursor {
                for x in area.x..area.x + content_width {
                    let cell = &mut buf[(x, y)];
                    if let Some(bg) = theme.selection.bg {
                        cell.set_bg(bg);
                    } else {
                        cell.modifier |= ratatui::style::Modifier::REVERSED;
                    }
                }
            }
            row += 1;

            // ── Child lines: progress, then server message ─────
            if let Some(ref progress) = server.progress_line
                && row < max_rows
            {
                let child = Line::from(vec![
                    Span::styled("  ", theme.muted),
                    Span::styled(progress.clone(), theme.accent),
                ]);
                buf.set_line(area.x, area.y + row as u16, &child, content_width);
                row += 1;
            }
            if let Some(ref msg) = server.server_message
                && row < max_rows
            {
                let child = Line::from(vec![
                    Span::styled("  ", theme.muted),
                    Span::styled(msg.clone(), theme.muted),
                ]);
                buf.set_line(area.x, area.y + row as u16, &child, content_width);
                row += 1;
            }
        }
    }

    // ── Vertical separator ──────────────────────────────────────────
    let sep_x = area.x + content_width;
    let sep_style = if sidebar_focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    for y in area.y..area.y + area.height {
        buf.set_string(sep_x, y, "│", sep_style);
    }

    hit_map
}

/// Choose a style for a lifecycle state string.
fn lifecycle_style(theme: &Theme, state: &str) -> ratatui::style::Style {
    match state {
        "ready" => theme.text,
        "busy" => theme.accent,
        _ => theme.muted,
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn host_label_claude() {
        assert_eq!(host_label(Some("claude-code")), "claude");
        assert_eq!(host_label(Some("claude")), "claude");
    }

    #[test]
    fn host_label_gemini() {
        assert_eq!(host_label(Some("gemini-cli")), "gemini");
        assert_eq!(host_label(Some("gemini")), "gemini");
    }

    #[test]
    fn host_label_unknown() {
        assert_eq!(host_label(Some("custom-agent")), "custom-agent");
    }

    #[test]
    fn host_label_none() {
        assert_eq!(host_label(None), "agent");
    }

    #[test]
    fn root_name_simple_path() {
        assert_eq!(root_name("/home/user/Projects/Catenary"), "Catenary/");
    }

    #[test]
    fn root_name_multi_root() {
        assert_eq!(
            root_name("/home/user/Catenary, /home/user/OmniDSP"),
            "Catenary/"
        );
    }

    #[test]
    fn root_name_trailing_slash() {
        assert_eq!(root_name("/home/user/Catenary/"), "Catenary/");
    }

    #[test]
    fn root_name_bare_name() {
        assert_eq!(root_name("Catenary"), "Catenary/");
    }

    #[test]
    fn content_width_empty() {
        let state = SidebarState::new();
        let w = state.content_width();
        assert!(w >= MIN_WIDTH, "empty sidebar should use MIN_WIDTH");
        assert!(w <= MAX_WIDTH);
    }

    #[test]
    fn content_width_auto_sizes() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![(
                "s1".into(),
                Some("claude-code".into()),
                "/Projects/CatenaryLongName".into(),
            )],
            &mut badges,
        );
        // "00 claude CatenaryLongName/" = 2+1+6+1+17 = 27, +1 sep = 28
        let w = state.content_width();
        assert!(w >= 20);
        assert!(w <= MAX_WIDTH);
    }

    #[test]
    fn content_width_clamped_to_max() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![(
                "s1".into(),
                Some("claude-code".into()),
                "/a/VeryLongWorkspaceRootNameThatExceedsMax".into(),
            )],
            &mut badges,
        );
        assert_eq!(state.content_width(), MAX_WIDTH);
    }

    #[test]
    fn refresh_adds_and_removes_sessions() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();

        // Add two sessions.
        state.refresh(
            vec![
                ("s1".into(), Some("claude-code".into()), "/tmp/A".into()),
                ("s2".into(), Some("gemini-cli".into()), "/tmp/B".into()),
            ],
            &mut badges,
        );
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0].badge, "00");
        assert_eq!(state.entries[1].badge, "01");

        // Remove s1, add s3.
        state.refresh(
            vec![
                ("s2".into(), Some("gemini-cli".into()), "/tmp/B".into()),
                ("s3".into(), Some("claude-code".into()), "/tmp/C".into()),
            ],
            &mut badges,
        );
        assert_eq!(state.entries.len(), 2);
        // s1's badge (00) was released and reused by s3.
        assert_eq!(state.entries[0].badge, "01"); // s2 keeps its badge
        assert_eq!(state.entries[1].badge, "00"); // s3 reuses s1's badge
    }

    #[test]
    fn needs_refresh_detects_changes() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();

        state.refresh(vec![("s1".into(), None, "/tmp/A".into())], &mut badges);

        assert!(!state.needs_refresh(&["s1".to_string()]));
        assert!(state.needs_refresh(&["s1".to_string(), "s2".to_string()]));
        assert!(state.needs_refresh(&[]));
    }

    #[test]
    fn cursor_clamps_on_removal() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();

        state.refresh(
            vec![
                ("s1".into(), None, "/tmp/A".into()),
                ("s2".into(), None, "/tmp/B".into()),
                ("s3".into(), None, "/tmp/C".into()),
            ],
            &mut badges,
        );
        state.cursor = 2;

        // Remove two sessions — cursor should clamp.
        state.refresh(vec![("s1".into(), None, "/tmp/A".into())], &mut badges);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn cursor_navigation() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                ("s1".into(), None, "/tmp/A".into()),
                ("s2".into(), None, "/tmp/B".into()),
                ("s3".into(), None, "/tmp/C".into()),
            ],
            &mut badges,
        );

        state.cursor_down(1, 10);
        assert_eq!(state.cursor, 1);
        state.cursor_down(5, 10);
        assert_eq!(state.cursor, 2, "should clamp to last entry");
        state.cursor_up(1, 10);
        assert_eq!(state.cursor, 1);
        state.cursor_up(5, 10);
        assert_eq!(state.cursor, 0, "should clamp to 0");
    }

    #[test]
    fn cursor_scrolls_when_entries_exceed_viewport() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                ("s1".into(), None, "/tmp/A".into()),
                ("s2".into(), None, "/tmp/B".into()),
                ("s3".into(), None, "/tmp/C".into()),
                ("s4".into(), None, "/tmp/D".into()),
                ("s5".into(), None, "/tmp/E".into()),
            ],
            &mut badges,
        );

        // Viewport fits only 3 entries.
        let visible = 3;
        assert_eq!(state.scroll_offset, 0);

        // Move down past the visible window.
        state.cursor_down(3, visible);
        assert_eq!(state.cursor, 3);
        assert_eq!(
            state.scroll_offset, 1,
            "should scroll to keep cursor visible"
        );

        state.cursor_down(1, visible);
        assert_eq!(state.cursor, 4);
        assert_eq!(state.scroll_offset, 2);

        // Move back up past the scroll offset.
        state.cursor_up(3, visible);
        assert_eq!(state.cursor, 1);
        assert_eq!(
            state.scroll_offset, 1,
            "should scroll up to keep cursor visible"
        );

        state.cursor_up(1, visible);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn render_sidebar_shows_entries() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                (
                    "s1".into(),
                    Some("claude-code".into()),
                    "/Projects/Catenary".into(),
                ),
                (
                    "s2".into(),
                    Some("gemini-cli".into()),
                    "/Projects/OmniDSP".into(),
                ),
            ],
            &mut badges,
        );

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(25, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(
                    &state,
                    area,
                    frame.buffer_mut(),
                    &theme,
                    FocusRegion::Sessions,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("Sessions"),
            "should show header: {content}"
        );
        assert!(content.contains("00"), "should show badge 00: {content}");
        assert!(content.contains("01"), "should show badge 01: {content}");
        assert!(content.contains("claude"), "should show host: {content}");
        assert!(content.contains("Catenary/"), "should show root: {content}");
    }

    #[test]
    fn render_sidebar_separator() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = SidebarState::new();
        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(22, 3);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(
                    &state,
                    area,
                    frame.buffer_mut(),
                    &theme,
                    FocusRegion::Stream,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);
        assert!(content.contains('│'), "should show separator: {content}");
    }

    #[test]
    fn render_sidebar_narrow_guard() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = SidebarState::new();
        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(2, 3);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(
                    &state,
                    area,
                    frame.buffer_mut(),
                    &theme,
                    FocusRegion::Sessions,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);
        let non_space = content.replace([' ', '\n'], "");
        assert!(
            non_space.is_empty(),
            "narrow sidebar should produce empty output, got: {content}"
        );
    }

    // ── Selection tests ──────────────────────────────────────────────

    #[test]
    fn toggle_selected_flips_state() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                ("s1".into(), None, "/tmp/A".into()),
                ("s2".into(), None, "/tmp/B".into()),
            ],
            &mut badges,
        );

        assert!(!state.has_filter(), "no filter initially");
        assert!(state.session_filter().is_none());

        // Select first entry.
        state.cursor = 0;
        assert!(state.toggle_selected());
        assert!(state.has_filter());
        assert!(state.is_selected("s1"));
        assert!(!state.is_selected("s2"));

        let filter = state.session_filter().expect("filter should be Some");
        assert!(filter.contains("s1"));
        assert!(!filter.contains("s2"));

        // Deselect first entry → back to show-all.
        assert!(state.toggle_selected());
        assert!(!state.has_filter());
        assert!(state.session_filter().is_none());
    }

    #[test]
    fn toggle_selected_multiple() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                ("s1".into(), None, "/tmp/A".into()),
                ("s2".into(), None, "/tmp/B".into()),
                ("s3".into(), None, "/tmp/C".into()),
            ],
            &mut badges,
        );

        state.cursor = 0;
        state.toggle_selected();
        state.cursor = 2;
        state.toggle_selected();

        let filter = state.session_filter().expect("filter should be Some");
        assert!(filter.contains("s1"));
        assert!(!filter.contains("s2"));
        assert!(filter.contains("s3"));
    }

    #[test]
    fn toggle_selected_empty_sidebar_is_noop() {
        let mut state = SidebarState::new();
        assert!(!state.toggle_selected());
        assert!(!state.has_filter());
    }

    #[test]
    fn refresh_clears_stale_selections() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                ("s1".into(), None, "/tmp/A".into()),
                ("s2".into(), None, "/tmp/B".into()),
            ],
            &mut badges,
        );

        // Select both.
        state.cursor = 0;
        state.toggle_selected();
        state.cursor = 1;
        state.toggle_selected();
        assert!(state.has_filter());

        // s2 disconnects.
        state.refresh(vec![("s1".into(), None, "/tmp/A".into())], &mut badges);

        // s2 selection should be cleared; s1 remains.
        assert!(state.has_filter());
        assert!(state.is_selected("s1"));
        assert!(!state.is_selected("s2"));
    }

    #[test]
    fn refresh_clears_filter_when_all_selected_disconnect() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![("s1".into(), None, "/tmp/A".into())], &mut badges);

        state.cursor = 0;
        state.toggle_selected();
        assert!(state.has_filter());

        // s1 disconnects — no sessions left.
        state.refresh(vec![], &mut badges);
        assert!(
            !state.has_filter(),
            "filter should clear when no sessions remain"
        );
    }

    // ── Render dim/bright tests ─────────────────────────────────────

    #[test]
    fn render_sidebar_selected_bright_unselected_dim() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Modifier;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                ("s1".into(), Some("claude-code".into()), "/tmp/A".into()),
                ("s2".into(), Some("gemini-cli".into()), "/tmp/B".into()),
            ],
            &mut badges,
        );

        // Select s1 only.
        state.cursor = 0;
        state.toggle_selected();

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(25, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(
                    &state,
                    area,
                    frame.buffer_mut(),
                    &theme,
                    FocusRegion::Sessions,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // Entry layout: "XX host Root/" where root starts at
        // badge(2) + space(1) + host(6) + space(1) = 10.
        let root_col = 10;

        // Row 1 (s1, selected): root text should NOT be dim.
        let s1_root_cell = &buf[(root_col, 1)];
        assert!(
            !s1_root_cell.modifier.contains(Modifier::DIM),
            "selected entry root should be bright, got modifiers: {:?}",
            s1_root_cell.modifier
        );

        // Row 2 (s2, unselected): root text should be dim.
        let s2_root_cell = &buf[(root_col, 2)];
        assert!(
            s2_root_cell.modifier.contains(Modifier::DIM),
            "unselected entry root should be dim, got modifiers: {:?}",
            s2_root_cell.modifier
        );
    }

    // ── Server tests ─────────────────────────────────────────────

    fn make_server_row(server: &str, scope_root: &str, state: &str) -> ServerStatusRow {
        ServerStatusRow {
            language_id: "rust".to_string(),
            server: server.to_string(),
            scope_kind: "root".to_string(),
            scope_root: scope_root.to_string(),
            state: state.to_string(),
        }
    }

    #[test]
    fn refresh_servers_one_entry_per_instance() {
        let mut state = SidebarState::new();
        let rows = vec![
            make_server_row("rust-analyzer", "/home/user/Catenary", "ready"),
            make_server_row("rust-analyzer", "/home/user/OmniDSP", "busy"),
            make_server_row("lua-ls", "/home/user/scripts", "ready"),
        ];

        state.refresh_servers(&rows, &[]);
        assert_eq!(state.servers.len(), 3);
        assert_eq!(state.servers[0].name, "rust-analyzer");
        assert_eq!(state.servers[0].root, "Catenary/");
        assert_eq!(state.servers[0].state, "ready");
        assert_eq!(state.servers[1].name, "rust-analyzer");
        assert_eq!(state.servers[1].root, "OmniDSP/");
        assert_eq!(state.servers[1].state, "busy");
        assert_eq!(state.servers[2].name, "lua-ls");
    }

    #[test]
    fn servers_need_refresh_detects_changes() {
        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "ready")];
        state.refresh_servers(&rows, &[]);

        assert!(!state.servers_need_refresh(&rows, &[]));

        let changed = vec![make_server_row("rust-analyzer", "/A", "busy")];
        assert!(state.servers_need_refresh(&changed, &[]));
    }

    #[test]
    fn separate_cursors_for_sessions_and_servers() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                ("s1".into(), None, "/tmp/A".into()),
                ("s2".into(), None, "/tmp/B".into()),
            ],
            &mut badges,
        );
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "busy"),
            ],
            &[],
        );

        // Session cursor is independent.
        state.cursor = 0;
        state.server_cursor = 1;
        assert_eq!(state.cursor, 0);
        assert_eq!(state.server_cursor, 1);

        // Navigate sessions without affecting servers.
        state.cursor_down(1, 10);
        assert_eq!(state.cursor, 1);
        assert_eq!(state.server_cursor, 1, "server cursor unchanged");

        // Navigate servers without affecting sessions.
        state.server_cursor_up(1, 10);
        assert_eq!(state.server_cursor, 0);
        assert_eq!(state.cursor, 1, "session cursor unchanged");
    }

    #[test]
    fn render_sidebar_shows_per_instance_servers() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/home/user/Catenary", "ready"),
                make_server_row("rust-analyzer", "/home/user/OmniDSP", "busy"),
            ],
            &[],
        );

        let theme = crate::tui::theme::Theme::new();
        // MAX_WIDTH is 30 — entries may truncate. Use MAX_WIDTH.
        let backend = TestBackend::new(MAX_WIDTH, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(
                    &state,
                    area,
                    frame.buffer_mut(),
                    &theme,
                    FocusRegion::Sessions,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("Servers"),
            "should show servers header: {content}"
        );
        assert!(
            content.contains("rust-analyzer"),
            "should show server name: {content}"
        );
        assert!(
            content.contains("Catenary/"),
            "should show root in entry: {content}"
        );
        assert!(
            content.contains("OmniDSP/"),
            "should show second instance root: {content}"
        );
        // Lifecycle state may be truncated by MAX_WIDTH — just verify
        // the entry includes the root scope to distinguish instances.
    }

    // ── Progress / server message extraction tests ────────────────

    fn make_noise(server: &str, method: &str, payload: serde_json::Value) -> ServerNoiseRow {
        ServerNoiseRow {
            server: server.to_string(),
            method: method.to_string(),
            payload,
        }
    }

    #[test]
    fn extract_progress_line_begin() {
        let noise = vec![make_noise(
            "rust-analyzer",
            "$/progress",
            serde_json::json!({
                "params": {
                    "value": {
                        "kind": "begin",
                        "title": "Indexing",
                        "percentage": 47
                    }
                }
            }),
        )];
        let line = extract_progress_line(&noise, "rust-analyzer");
        assert_eq!(line.as_deref(), Some("Indexing… 47%"));
    }

    #[test]
    fn extract_progress_line_end_returns_none() {
        let noise = vec![make_noise(
            "rust-analyzer",
            "$/progress",
            serde_json::json!({
                "params": {
                    "value": { "kind": "end" }
                }
            }),
        )];
        assert!(extract_progress_line(&noise, "rust-analyzer").is_none());
    }

    #[test]
    fn extract_progress_line_wrong_server() {
        let noise = vec![make_noise(
            "lua-ls",
            "$/progress",
            serde_json::json!({
                "params": {
                    "value": { "kind": "begin", "title": "Loading" }
                }
            }),
        )];
        assert!(extract_progress_line(&noise, "rust-analyzer").is_none());
    }

    #[test]
    fn extract_server_message_log() {
        let noise = vec![make_noise(
            "rust-analyzer",
            "window/logMessage",
            serde_json::json!({
                "params": { "message": "Fetching crate data" }
            }),
        )];
        let msg = extract_server_message(&noise, "rust-analyzer");
        assert_eq!(msg.as_deref(), Some("Fetching crate data"));
    }

    #[test]
    fn extract_server_message_prefers_show_over_log() {
        let noise = vec![
            make_noise(
                "rust-analyzer",
                "window/logMessage",
                serde_json::json!({
                    "params": { "message": "log message" }
                }),
            ),
            make_noise(
                "rust-analyzer",
                "window/showMessage",
                serde_json::json!({
                    "params": { "message": "show message" }
                }),
            ),
        ];
        let msg = extract_server_message(&noise, "rust-analyzer");
        assert_eq!(msg.as_deref(), Some("show message"));
    }

    #[test]
    fn total_rows_includes_child_lines() {
        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "busy")];
        let noise = vec![make_noise(
            "rust-analyzer",
            "$/progress",
            serde_json::json!({
                "params": {
                    "value": { "kind": "begin", "title": "Indexing", "percentage": 50 }
                }
            }),
        )];
        state.refresh_servers(&rows, &noise);

        // Sessions header(1) + blank(1) + servers header(1) + server(1) + progress child(1) = 5
        assert_eq!(state.total_rows(), 5);
    }

    #[test]
    fn servers_need_refresh_detects_noise_change() {
        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "busy")];
        let noise = vec![make_noise(
            "rust-analyzer",
            "$/progress",
            serde_json::json!({
                "params": { "value": { "kind": "begin", "title": "Indexing", "percentage": 10 } }
            }),
        )];
        state.refresh_servers(&rows, &noise);

        // Same noise — no refresh needed.
        assert!(!state.servers_need_refresh(&rows, &noise));

        // Changed percentage — refresh needed.
        let noise2 = vec![make_noise(
            "rust-analyzer",
            "$/progress",
            serde_json::json!({
                "params": { "value": { "kind": "begin", "title": "Indexing", "percentage": 50 } }
            }),
        )];
        assert!(state.servers_need_refresh(&rows, &noise2));
    }

    #[test]
    fn render_sidebar_shows_progress_child() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "busy")];
        let noise = vec![make_noise(
            "rust-analyzer",
            "$/progress",
            serde_json::json!({
                "params": {
                    "value": { "kind": "begin", "title": "Indexing", "percentage": 47 }
                }
            }),
        )];
        state.refresh_servers(&rows, &noise);

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(MAX_WIDTH, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(
                    &state,
                    area,
                    frame.buffer_mut(),
                    &theme,
                    FocusRegion::Sessions,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("rust-analyzer"),
            "should show server: {content}"
        );
        assert!(
            content.contains("Indexing"),
            "should show progress title: {content}"
        );
        assert!(
            content.contains("47%"),
            "should show progress percentage: {content}"
        );
    }

    // ── Server selection tests ──────────────────────────────────────

    #[test]
    fn toggle_server_selected_flips_state() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
            ],
            &[],
        );

        assert!(!state.has_server_filter(), "no filter initially");
        assert!(state.server_filter().is_none());

        // Select first server.
        state.server_cursor = 0;
        assert!(state.toggle_server_selected());
        assert!(state.has_server_filter());
        assert!(state.is_server_selected("rust-analyzer", "/A"));
        assert!(!state.is_server_selected("lua-ls", "/B"));

        // Deselect → back to show-all.
        assert!(state.toggle_server_selected());
        assert!(!state.has_server_filter());
    }

    #[test]
    fn toggle_server_selects_per_instance() {
        let mut state = SidebarState::new();
        // Two instances of rust-analyzer (different roots).
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("rust-analyzer", "/B", "busy"),
                make_server_row("lua-ls", "/C", "ready"),
            ],
            &[],
        );

        // Select first rust-analyzer instance (root /A).
        state.server_cursor = 0;
        state.toggle_server_selected();

        // Only the selected instance matches.
        assert!(state.is_server_selected("rust-analyzer", "/A"));
        // Second instance (root /B) is NOT selected.
        assert!(!state.is_server_selected("rust-analyzer", "/B"));
        assert!(!state.is_server_selected("lua-ls", "/C"));
    }

    #[test]
    fn toggle_server_selected_empty_is_noop() {
        let mut state = SidebarState::new();
        assert!(!state.toggle_server_selected());
        assert!(!state.has_server_filter());
    }

    #[test]
    fn refresh_servers_clears_stale_selections() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
            ],
            &[],
        );

        // Select both.
        state.server_cursor = 0;
        state.toggle_server_selected();
        state.server_cursor = 1;
        state.toggle_server_selected();
        assert!(state.has_server_filter());

        // lua-ls disappears.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);

        assert!(state.has_server_filter());
        assert!(state.is_server_selected("rust-analyzer", "/A"));
        assert!(!state.is_server_selected("lua-ls", "/B"));
    }

    #[test]
    fn server_cursor_navigation() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
                make_server_row("pyright", "/C", "ready"),
            ],
            &[],
        );

        state.server_cursor_down(1, 10);
        assert_eq!(state.server_cursor, 1);
        state.server_cursor_down(5, 10);
        assert_eq!(state.server_cursor, 2, "should clamp to last server");
        state.server_cursor_up(1, 10);
        assert_eq!(state.server_cursor, 1);
        state.server_cursor_up(5, 10);
        assert_eq!(state.server_cursor, 0, "should clamp to 0");
    }

    #[test]
    fn render_sidebar_server_selected_bright_unselected_dim() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Modifier;

        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
            ],
            &[],
        );

        // Select rust-analyzer only.
        state.server_cursor = 0;
        state.toggle_server_selected();

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(MAX_WIDTH, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(
                    &state,
                    area,
                    frame.buffer_mut(),
                    &theme,
                    FocusRegion::Servers,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // Sessions header = row 0, no sessions, blank = row 1,
        // Servers header = row 2, rust-analyzer = row 3, lua-ls = row 4
        let ra_cell = &buf[(0, 3)]; // "r" in "rust-analyzer"
        let lua_cell = &buf[(0, 4)]; // "l" in "lua-ls"

        assert!(
            !ra_cell.modifier.contains(Modifier::DIM),
            "selected server should be bright, got: {:?}",
            ra_cell.modifier
        );
        assert!(
            lua_cell.modifier.contains(Modifier::DIM),
            "unselected server should be dim, got: {:?}",
            lua_cell.modifier
        );
    }

    // ── Helpers ─────────────────────────────────────────────────────

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
