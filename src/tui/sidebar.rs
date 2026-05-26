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

use super::data::ServerStatusRow;
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

/// A server entry in the sidebar, grouped by server name.
///
/// Multi-root servers (one process, multiple roots via `workspaceFolders`)
/// appear as one entry with multiple root children. Per-root servers
/// (separate instance per root) also group under one name, showing the
/// most significant lifecycle state on the header line.
pub struct ServerEntry {
    /// Server binary name (config key, e.g., "rust-analyzer").
    pub name: String,
    /// Aggregated lifecycle display state for the header line.
    /// Uses the most significant state when instances differ.
    pub state: String,
    /// Workspace root short names served by this server.
    pub roots: Vec<String>,
    /// Whether the children (roots) are visible.
    pub expanded: bool,
}

impl ServerEntry {
    /// Display width of the header line: `<name>  <state>`.
    const fn header_width(&self) -> usize {
        // name + 2 spaces + state
        self.name.len() + 2 + self.state.len()
    }
}

// ── Sidebar state ────────────────────────────────────────────────────

/// Sidebar state: session list, server list, navigation cursor, and
/// selection filter.
pub struct SidebarState {
    /// Alive sessions in display order.
    pub entries: Vec<SessionEntry>,
    /// Active server entries, grouped by server name.
    pub servers: Vec<ServerEntry>,
    /// Cursor position in the session list.
    pub cursor: usize,
    /// First visible entry index (scroll offset).
    pub scroll_offset: usize,
    /// Session IDs from the last refresh (for change detection).
    last_ids: Vec<String>,
    /// Server names from the last refresh (for change detection).
    last_server_names: Vec<String>,
    /// Selected session IDs (for stream filtering).
    /// Empty = show all (no filter active).
    selected: HashSet<String>,
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
            last_ids: Vec::new(),
            last_server_names: Vec::new(),
            selected: HashSet::new(),
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

        // Clamp cursor to total item count.
        let max = self.item_count().saturating_sub(1);
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

    /// Check whether the server list has changed since the last refresh.
    #[must_use]
    pub fn servers_need_refresh(&self, rows: &[ServerStatusRow]) -> bool {
        // Quick check: compare sorted server names + states.
        let current: Vec<String> = rows
            .iter()
            .map(|r| format!("{}:{}", r.server, r.state))
            .collect();
        self.last_server_names != current
    }

    /// Update the server list from DB rows.
    ///
    /// Groups rows by server name. Each group becomes one sidebar entry
    /// with workspace root short names as expandable children.
    pub fn refresh_servers(&mut self, rows: &[ServerStatusRow]) {
        self.last_server_names = rows
            .iter()
            .map(|r| format!("{}:{}", r.server, r.state))
            .collect();

        // Preserve expansion state across refreshes.
        let was_expanded: HashSet<String> = self
            .servers
            .iter()
            .filter(|s| s.expanded)
            .map(|s| s.name.clone())
            .collect();

        // Group by server name, preserving input order.
        let mut groups: Vec<(String, String, Vec<String>)> = Vec::new();
        for row in rows {
            let root = server_root_name(&row.scope_root);
            if let Some(group) = groups.iter_mut().find(|(name, _, _)| *name == row.server) {
                if !root.is_empty() && !group.2.contains(&root) {
                    group.2.push(root);
                }
                // Update state to most significant.
                group.1 = most_significant_state(&group.1, &row.state);
            } else {
                let roots = if root.is_empty() {
                    Vec::new()
                } else {
                    vec![root]
                };
                groups.push((row.server.clone(), row.state.clone(), roots));
            }
        }

        self.servers = groups
            .into_iter()
            .map(|(name, state, roots)| {
                let expanded = was_expanded.contains(&name);
                ServerEntry {
                    name,
                    state,
                    roots,
                    expanded,
                }
            })
            .collect();
    }

    /// Toggle expand/collapse on the server entry at the cursor.
    ///
    /// Only applies when the cursor points to a server header line
    /// (not a session entry). Returns `true` if state changed.
    pub fn toggle_server_expansion(&mut self, cursor_in_servers: usize) -> bool {
        if let Some(entry) = self.servers.get_mut(cursor_in_servers) {
            entry.expanded = !entry.expanded;
            true
        } else {
            false
        }
    }

    /// Total number of navigable items (sessions + servers).
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.entries.len() + self.servers.len()
    }

    /// Whether the cursor is on a session entry.
    #[must_use]
    pub const fn cursor_on_session(&self) -> bool {
        self.cursor < self.entries.len()
    }

    /// Server index if cursor is on a server entry.
    #[must_use]
    pub const fn cursor_server_index(&self) -> Option<usize> {
        if self.cursor >= self.entries.len() {
            Some(self.cursor - self.entries.len())
        } else {
            None
        }
    }

    /// Move cursor up by `n` entries, scrolling if needed.
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

    /// Move cursor down by `n` entries, scrolling if needed.
    ///
    /// `visible` is the number of entry rows visible in the sidebar.
    pub fn cursor_down(&mut self, n: usize, visible: usize) {
        let total = self.item_count();
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
    /// server entries (with expanded children).
    #[must_use]
    pub fn total_rows(&self) -> usize {
        let session_rows = 1 + self.entries.len(); // header + entries
        let server_header = if self.servers.is_empty() { 0 } else { 2 }; // blank + header
        let server_rows: usize = self
            .servers
            .iter()
            .map(|s| {
                if s.expanded {
                    1 + s.roots.len() // header + roots
                } else {
                    1 // header only
                }
            })
            .sum();
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

/// Return the more significant of two lifecycle display states.
///
/// Priority: `busy` > `initializing` > `ready` > `dead`.
fn most_significant_state<'a>(a: &'a str, b: &'a str) -> String {
    fn rank(s: &str) -> u8 {
        match s {
            "busy" => 3,
            "initializing" => 2,
            "ready" => 1,
            _ => 0,
        }
    }
    if rank(b) > rank(a) {
        b.to_string()
    } else {
        a.to_string()
    }
}

// ── Rendering ────────────────────────────────────────────────────────

/// Render the sidebar into the given area.
///
/// The rightmost column of `area` renders a vertical separator (`│`).
/// The rest shows two sections: "Sessions" (top) and "Servers" (bottom),
/// separated by a blank line. Focus state controls header style and
/// cursor highlight visibility.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_sidebar(
    state: &SidebarState,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    focused: bool,
) {
    if area.width < 3 || area.height == 0 {
        return;
    }

    // Reserve rightmost column for the vertical separator.
    let content_width = area.width.saturating_sub(1);
    let header_style = if focused { theme.title } else { theme.muted };

    let max_rows = area.height as usize;
    let mut row: usize = 0;

    // ── Sessions header ─────────────────────────────────────────────
    let header = Line::from(Span::styled("Sessions", header_style));
    buf.set_line(area.x, area.y + row as u16, &header, content_width);
    row += 1;

    // ── Session entries ─────────────────────────────────────────────
    let has_filter = state.has_filter();
    for (i, entry) in state.entries.iter().enumerate() {
        if row >= max_rows {
            break;
        }

        let is_cursor = focused && i == state.cursor;

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
            let server_header = Line::from(Span::styled("Servers", header_style));
            buf.set_line(area.x, area.y + row as u16, &server_header, content_width);
            row += 1;
        }

        let sessions_len = state.entries.len();
        for (si, server) in state.servers.iter().enumerate() {
            if row >= max_rows {
                break;
            }

            let is_cursor = focused && state.cursor == sessions_len + si;

            // Server header: name + state
            let state_style = lifecycle_style(theme, &server.state);
            let line = Line::from(vec![
                Span::styled(&server.name, theme.text),
                Span::raw("  "),
                Span::styled(&server.state, state_style),
            ]);
            let y = area.y + row as u16;
            buf.set_line(area.x, y, &line, content_width);

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

            if server.expanded {
                // Show workspace roots as indented children.
                for root in &server.roots {
                    if row >= max_rows {
                        break;
                    }
                    let line = Line::from(vec![
                        Span::raw("  "),
                        Span::styled(root.as_str(), theme.muted),
                    ]);
                    buf.set_line(area.x, area.y + row as u16, &line, content_width);
                    row += 1;
                }
            }
        }
    }

    // ── Vertical separator ──────────────────────────────────────────
    let sep_x = area.x + content_width;
    let sep_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    for y in area.y..area.y + area.height {
        buf.set_string(sep_x, y, "│", sep_style);
    }
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
                render_sidebar(&state, area, frame.buffer_mut(), &theme, true);
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
                render_sidebar(&state, area, frame.buffer_mut(), &theme, false);
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
                render_sidebar(&state, area, frame.buffer_mut(), &theme, true);
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
                render_sidebar(&state, area, frame.buffer_mut(), &theme, true);
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
    fn refresh_servers_groups_by_name() {
        let mut state = SidebarState::new();
        let rows = vec![
            make_server_row("rust-analyzer", "/home/user/Catenary", "ready"),
            make_server_row("rust-analyzer", "/home/user/OmniDSP", "ready"),
            make_server_row("lua-ls", "/home/user/scripts", "busy"),
        ];

        state.refresh_servers(&rows);
        assert_eq!(state.servers.len(), 2);
        assert_eq!(state.servers[0].name, "rust-analyzer");
        assert_eq!(state.servers[0].roots.len(), 2);
        assert!(state.servers[0].roots.contains(&"Catenary/".to_string()));
        assert!(state.servers[0].roots.contains(&"OmniDSP/".to_string()));
        assert_eq!(state.servers[1].name, "lua-ls");
        assert_eq!(state.servers[1].roots.len(), 1);
    }

    #[test]
    fn refresh_servers_most_significant_state() {
        let mut state = SidebarState::new();
        let rows = vec![
            make_server_row("rust-analyzer", "/home/user/A", "ready"),
            make_server_row("rust-analyzer", "/home/user/B", "busy"),
        ];

        state.refresh_servers(&rows);
        assert_eq!(state.servers[0].state, "busy");
    }

    #[test]
    fn refresh_servers_preserves_expansion() {
        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/home/user/A", "ready")];
        state.refresh_servers(&rows);

        // Expand it.
        state.servers[0].expanded = true;

        // Refresh again — expansion should be preserved.
        let rows2 = vec![make_server_row("rust-analyzer", "/home/user/A", "busy")];
        state.refresh_servers(&rows2);
        assert!(
            state.servers[0].expanded,
            "expansion should persist across refresh"
        );
    }

    #[test]
    fn servers_need_refresh_detects_changes() {
        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "ready")];
        state.refresh_servers(&rows);

        assert!(!state.servers_need_refresh(&rows));

        let changed = vec![make_server_row("rust-analyzer", "/A", "busy")];
        assert!(state.servers_need_refresh(&changed));
    }

    #[test]
    fn cursor_spans_sessions_and_servers() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![("s1".into(), None, "/tmp/A".into())], &mut badges);
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")]);

        assert_eq!(state.item_count(), 2);

        // Cursor on session.
        state.cursor = 0;
        assert!(state.cursor_on_session());
        assert!(state.cursor_server_index().is_none());

        // Cursor on server.
        state.cursor = 1;
        assert!(!state.cursor_on_session());
        assert_eq!(state.cursor_server_index(), Some(0));
    }

    #[test]
    fn toggle_server_expansion() {
        let mut state = SidebarState::new();
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")]);

        assert!(!state.servers[0].expanded);
        assert!(state.toggle_server_expansion(0));
        assert!(state.servers[0].expanded);
        assert!(state.toggle_server_expansion(0));
        assert!(!state.servers[0].expanded);
    }

    #[test]
    fn render_sidebar_shows_servers() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        state.refresh_servers(&[
            make_server_row("rust-analyzer", "/home/user/Catenary", "ready"),
            make_server_row("lua-ls", "/home/user/scripts", "busy"),
        ]);

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(25, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(&state, area, frame.buffer_mut(), &theme, true);
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
            content.contains("ready"),
            "should show lifecycle state: {content}"
        );
        assert!(
            content.contains("lua-ls"),
            "should show second server: {content}"
        );
    }

    #[test]
    fn render_sidebar_expanded_shows_roots() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        state.refresh_servers(&[
            make_server_row("rust-analyzer", "/home/user/Catenary", "ready"),
            make_server_row("rust-analyzer", "/home/user/OmniDSP", "ready"),
        ]);
        state.servers[0].expanded = true;

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(25, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sidebar(&state, area, frame.buffer_mut(), &theme, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("Catenary/"),
            "expanded server should show root: {content}"
        );
        assert!(
            content.contains("OmniDSP/"),
            "expanded server should show second root: {content}"
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
