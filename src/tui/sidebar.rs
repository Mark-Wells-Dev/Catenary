// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Sidebar widget: session filter list and server dashboard.
//!
//! Built in tickets 03–07. This module provides the session list
//! sidebar with hex badges, host format labels, and primary root
//! names. Sessions appear on connect and disappear on disconnect.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::data::{ServerNoiseRow, ServerStatusRow};
use super::stream::HexBadgeMap;
use super::theme::Theme;

/// Number of columns to scroll per horizontal scroll step.
const HSCROLL_STEP: u16 = 4;

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
    /// Full workspace path(s), comma-separated.
    pub workspace: String,
    /// Active language servers (e.g., `["rust-analyzer", "lua-language-server"]`).
    pub languages: Vec<String>,
}

/// Input data for sidebar session refresh.
pub struct SessionData {
    /// Session ID (database key).
    pub id: String,
    /// Host CLI client name (e.g., `"claude-code"`).
    pub client_name: Option<String>,
    /// Full workspace path(s), comma-separated.
    pub workspace: String,
    /// Active language server names.
    pub languages: Vec<String>,
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
    /// Servers observed to die during this TUI session.
    ///
    /// Accumulated below a thematic break in the Servers panel. No
    /// backfilling — only servers that were live and then disappeared
    /// (or transitioned to "dead") while the TUI was running.
    pub dead_servers: Vec<ServerEntry>,
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
    /// Horizontal scroll offset for session entries (in columns).
    session_hscroll: u16,
    /// Horizontal scroll offset for server entries (in columns).
    server_hscroll: u16,
    /// Session IDs with expanded detail rows.
    expanded_sessions: HashSet<String>,
    /// Visual selection anchor for sessions (entry index).
    session_visual_anchor: Option<usize>,
    /// Visual selection anchor for servers (entry index).
    server_visual_anchor: Option<usize>,
}

impl SidebarState {
    /// Create an empty sidebar.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            servers: Vec::new(),
            dead_servers: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            server_cursor: 0,
            server_scroll_offset: 0,
            last_ids: Vec::new(),
            last_server_names: Vec::new(),
            selected: HashSet::new(),
            selected_servers: HashSet::new(),
            session_hscroll: 0,
            server_hscroll: 0,
            expanded_sessions: HashSet::new(),
            session_visual_anchor: None,
            server_visual_anchor: None,
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
    /// Accepts `SessionData` records for alive sessions only. Releases
    /// badges for sessions that have disconnected and assigns badges
    /// for new ones.
    pub fn refresh(&mut self, sessions: Vec<SessionData>, badges: &mut HexBadgeMap) {
        // Release badges for removed sessions and prune stale selections.
        let new_ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        for entry in &self.entries {
            if !new_ids.contains(entry.session_id.as_str()) {
                badges.release(&entry.session_id);
            }
        }
        self.selected.retain(|id| new_ids.contains(id.as_str()));
        self.expanded_sessions
            .retain(|id| new_ids.contains(id.as_str()));
        drop(new_ids);

        self.entries = sessions
            .into_iter()
            .map(|s| {
                let badge = badges.badge(&s.id);
                let host = s.client_name.unwrap_or_else(|| "agent".to_string());
                let root = root_name(&s.workspace);
                SessionEntry {
                    session_id: s.id,
                    badge,
                    host,
                    root,
                    workspace: s.workspace,
                    languages: s.languages,
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

    /// Toggle expansion on the session at the current cursor position.
    pub fn toggle_session_expanded(&mut self) {
        let Some(entry) = self.entries.get(self.cursor) else {
            return;
        };
        let id = &entry.session_id;
        if self.expanded_sessions.contains(id) {
            self.expanded_sessions.remove(id);
        } else {
            self.expanded_sessions.insert(id.clone());
        }
    }

    /// Whether a specific session is expanded.
    #[must_use]
    pub fn is_expanded(&self, session_id: &str) -> bool {
        self.expanded_sessions.contains(session_id)
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
            current.push(format!(
                "noise:{}:{}:{:?}:{:?}:{:?}",
                n.server, n.scope_root, n.progress_title, n.progress_pct, n.last_message
            ));
        }
        self.last_server_names != current
    }

    /// Update the server list from DB rows and noise data.
    ///
    /// Each status row becomes one sidebar entry — one entry per server
    /// process. Noise rows populate progress and server message children.
    /// Servers that were previously live but are absent from the new set
    /// are accumulated in `dead_servers`. Servers that reappear live are
    /// removed from the dead list. Prunes stale server selections.
    pub fn refresh_servers(&mut self, rows: &[ServerStatusRow], noise: &[ServerNoiseRow]) {
        self.last_server_names = rows
            .iter()
            .map(|r| format!("{}:{}:{}", r.server, r.scope_root, r.state))
            .collect();
        for n in noise {
            self.last_server_names.push(format!(
                "noise:{}:{}:{:?}:{:?}:{:?}",
                n.server, n.scope_root, n.progress_title, n.progress_pct, n.last_message
            ));
        }

        let new_keys: HashSet<(&str, &str)> = rows
            .iter()
            .map(|r| (r.server.as_str(), r.scope_root.as_str()))
            .collect();

        // Detect servers that were live and are now gone → accumulate as dead.
        // Only accumulate if there was a previous refresh (non-empty servers list
        // or non-empty dead list indicates we've been running).
        if !self.servers.is_empty() {
            for server in &self.servers {
                let key = (server.name.as_str(), server.scope_root.as_str());
                if !new_keys.contains(&key) {
                    // Only add if not already in dead list.
                    let already_dead = self
                        .dead_servers
                        .iter()
                        .any(|d| d.name == server.name && d.scope_root == server.scope_root);
                    if !already_dead {
                        self.dead_servers.push(ServerEntry {
                            name: server.name.clone(),
                            scope_root: server.scope_root.clone(),
                            root: server.root.clone(),
                            state: "dead".to_string(),
                            progress_line: None,
                            server_message: None,
                        });
                    }
                }
            }
        }

        // Remove from dead list any servers that reappeared live.
        self.dead_servers
            .retain(|d| !new_keys.contains(&(d.name.as_str(), d.scope_root.as_str())));

        self.servers = rows
            .iter()
            .map(|row| {
                let progress_line = extract_progress_line(noise, &row.server, &row.scope_root);
                let server_message = extract_server_message(noise, &row.server, &row.scope_root);
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

    // ── Horizontal scroll ─────────────────────────────────────────────

    /// Scroll session entries left by one step.
    pub const fn hscroll_sessions_left(&mut self) {
        self.session_hscroll = self.session_hscroll.saturating_sub(HSCROLL_STEP);
    }

    /// Scroll session entries right by one step.
    pub const fn hscroll_sessions_right(&mut self) {
        self.session_hscroll = self.session_hscroll.saturating_add(HSCROLL_STEP);
    }

    /// Scroll server entries left by one step.
    pub const fn hscroll_servers_left(&mut self) {
        self.server_hscroll = self.server_hscroll.saturating_sub(HSCROLL_STEP);
    }

    /// Scroll server entries right by one step.
    pub const fn hscroll_servers_right(&mut self) {
        self.server_hscroll = self.server_hscroll.saturating_add(HSCROLL_STEP);
    }

    // ── Session visual selection ──────────────────────────────────────

    /// Enter visual selection mode for sessions, anchoring at the cursor.
    pub const fn start_session_visual(&mut self) {
        self.session_visual_anchor = Some(self.cursor);
    }

    /// Exit visual selection mode for sessions.
    pub const fn exit_session_visual(&mut self) {
        self.session_visual_anchor = None;
    }

    /// Whether session visual selection is active.
    #[must_use]
    pub const fn in_session_visual(&self) -> bool {
        self.session_visual_anchor.is_some()
    }

    /// Inclusive range of the session visual selection.
    #[must_use]
    pub const fn session_visual_range(&self) -> Option<(usize, usize)> {
        let Some(anchor) = self.session_visual_anchor else {
            return None;
        };
        if anchor <= self.cursor {
            Some((anchor, self.cursor))
        } else {
            Some((self.cursor, anchor))
        }
    }

    /// Plain text for a single session entry (for yank).
    fn session_plain_text(&self, idx: usize) -> Option<String> {
        let entry = self.entries.get(idx)?;
        let mut text = format!("{} {} {}", entry.badge, entry.host, entry.root);
        if self.is_expanded(&entry.session_id) {
            let _ = write!(text, "\n  {}", entry.workspace);
            if !entry.languages.is_empty() {
                let _ = write!(text, "\n  {}", entry.languages.join(", "));
            }
        }
        Some(text)
    }

    /// Yank text for the session visual selection (or cursor entry).
    #[must_use]
    pub fn yank_sessions_text(&self) -> Option<String> {
        let (start, end) = self
            .session_visual_range()
            .unwrap_or((self.cursor, self.cursor));
        let lines: Vec<String> = (start..=end)
            .filter_map(|i| self.session_plain_text(i))
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    // ── Server visual selection ───────────────────────────────────────

    /// Enter visual selection mode for servers, anchoring at the cursor.
    pub const fn start_server_visual(&mut self) {
        self.server_visual_anchor = Some(self.server_cursor);
    }

    /// Exit visual selection mode for servers.
    pub const fn exit_server_visual(&mut self) {
        self.server_visual_anchor = None;
    }

    /// Whether server visual selection is active.
    #[must_use]
    pub const fn in_server_visual(&self) -> bool {
        self.server_visual_anchor.is_some()
    }

    /// Inclusive range of the server visual selection.
    #[must_use]
    pub const fn server_visual_range(&self) -> Option<(usize, usize)> {
        let Some(anchor) = self.server_visual_anchor else {
            return None;
        };
        if anchor <= self.server_cursor {
            Some((anchor, self.server_cursor))
        } else {
            Some((self.server_cursor, anchor))
        }
    }

    /// Plain text for a single server entry (for yank).
    fn server_plain_text(&self, idx: usize) -> Option<String> {
        let entry = self.servers.get(idx)?;
        let mut text = if entry.root.is_empty() {
            format!("{}  {}", entry.name, entry.state)
        } else {
            format!("{} ({})  {}", entry.name, entry.root, entry.state)
        };
        if let Some(ref progress) = entry.progress_line {
            let _ = write!(text, "\n  {progress}");
        }
        if let Some(ref msg) = entry.server_message {
            let _ = write!(text, "\n  {msg}");
        }
        Some(text)
    }

    /// Yank text for the server visual selection (or cursor entry).
    #[must_use]
    pub fn yank_servers_text(&self) -> Option<String> {
        let (start, end) = self
            .server_visual_range()
            .unwrap_or((self.server_cursor, self.server_cursor));
        let lines: Vec<String> = (start..=end)
            .filter_map(|i| self.server_plain_text(i))
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

impl Default for SidebarState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Label helpers ────────────────────────────────────────────────────

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

/// Extract a progress line from noise data for the given server instance.
///
/// Reads pre-computed `progress_title` and `progress_pct` columns from
/// the `language_servers` table. Returns a display string like
/// `"Indexing… 47%"`, or `None` if no active progress.
fn extract_progress_line(
    noise: &[ServerNoiseRow],
    server: &str,
    scope_root: &str,
) -> Option<String> {
    let row = noise
        .iter()
        .find(|n| n.server == server && n.scope_root == scope_root && n.progress_title.is_some())?;

    let title = row.progress_title.as_deref()?;
    Some(
        row.progress_pct
            .map_or_else(|| format!("{title}…"), |pct| format!("{title}… {pct}%")),
    )
}

/// Extract the most recent server message from noise data for a server instance.
///
/// Reads the pre-computed `last_message` column from the `language_servers`
/// table.
fn extract_server_message(
    noise: &[ServerNoiseRow],
    server: &str,
    scope_root: &str,
) -> Option<String> {
    let row = noise
        .iter()
        .find(|n| n.server == server && n.scope_root == scope_root && n.last_message.is_some())?;
    row.last_message.clone()
}

// ── Horizontal scroll helpers ────────────────────────────────────────

/// Width of the `"…"` indicator shown when content is scrolled right.
const HSCROLL_IND_WIDTH: u16 = 1;

/// Render a line into the buffer, applying horizontal scroll.
///
/// When `hscroll` is 0 this is a plain `buf.set_line`. When non-zero,
/// an `"…"` indicator is drawn at the left edge and content is shifted
/// by `hscroll` columns so the user can see text clipped by the panel.
fn set_line_scrolled(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    line: &Line<'_>,
    hscroll: u16,
    muted: Style,
) {
    if hscroll == 0 {
        buf.set_line(area.x, y, line, area.width);
        return;
    }
    let ind = Line::from(Span::styled("\u{2026}", muted));
    buf.set_line(area.x, y, &ind, area.width);
    let scrolled = hscroll_line(line, hscroll);
    buf.set_line(
        area.x + HSCROLL_IND_WIDTH,
        y,
        &scrolled,
        area.width.saturating_sub(HSCROLL_IND_WIDTH),
    );
}

/// Produce a new `Line` with the first `hscroll` columns removed.
///
/// Walks through spans character-by-character using unicode column
/// widths. Spans fully consumed by the offset are dropped. A span
/// partially consumed is trimmed to the remaining characters.
fn hscroll_line(line: &Line<'_>, hscroll: u16) -> Line<'static> {
    let mut remaining = usize::from(hscroll);
    let mut spans: Vec<Span<'static>> = Vec::new();

    for span in &line.spans {
        if remaining == 0 {
            spans.push(Span::styled(span.content.to_string(), span.style));
            continue;
        }

        let mut char_iter = span.content.chars();
        loop {
            if remaining == 0 {
                break;
            }
            match char_iter.next() {
                Some(c) => {
                    let w = UnicodeWidthChar::width(c).unwrap_or(0);
                    remaining = remaining.saturating_sub(w);
                }
                None => break,
            }
        }

        let rest: String = char_iter.collect();
        if !rest.is_empty() {
            spans.push(Span::styled(rest, span.style));
        }
    }

    Line::from(spans)
}

// ── Rendering ────────────────────────────────────────────────────────

/// Apply a highlight style to every cell in a row.
///
/// If the style has an explicit background color, sets `bg`; otherwise
/// applies `REVERSED` so the terminal's own colors are used.
fn apply_highlight(buf: &mut Buffer, area: Rect, y: u16, style: &Style) {
    for x in area.x..area.x + area.width {
        let cell = &mut buf[(x, y)];
        if let Some(bg) = style.bg {
            cell.set_bg(bg);
        } else {
            cell.modifier |= ratatui::style::Modifier::REVERSED;
        }
    }
}

/// Render session entries into the given area (inside a `Block` frame).
///
/// No header text — the `Block` title replaces it. No vertical separator.
/// Returns a mapping from terminal row to session index for mouse clicks.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_sessions(
    state: &SidebarState,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    focused: bool,
) -> Vec<(u16, usize)> {
    let mut hits: Vec<(u16, usize)> = Vec::new();

    if area.width == 0 || area.height == 0 {
        return hits;
    }

    let max_rows = area.height as usize;
    let has_filter = state.has_filter();
    let mut row: usize = 0;

    // Clamp hscroll so the user can't scroll past the longest entry.
    // Each entry: badge(2) + " " + host + " " + root.
    let max_content: u16 = state
        .entries
        .iter()
        .map(|e| (e.badge.width() + 1 + e.host.width() + 1 + e.root.width()) as u16)
        .max()
        .unwrap_or(0);
    let hs = state
        .session_hscroll
        .min(max_content.saturating_sub(area.width));

    for (i, entry) in state.entries.iter().enumerate() {
        if row >= max_rows {
            break;
        }

        let is_cursor = focused && i == state.cursor;
        let in_visual = focused
            && state
                .session_visual_range()
                .is_some_and(|(s, e)| i >= s && i <= e);

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
        set_line_scrolled(buf, area, y, &line, hs, theme.muted);
        hits.push((y, i));

        let highlight = if is_cursor {
            Some(&theme.selection)
        } else if in_visual {
            Some(&theme.visual_selection)
        } else {
            None
        };

        if let Some(style) = highlight {
            apply_highlight(buf, area, y, style);
        }
        row += 1;

        // ── Child lines: expanded detail ──────────────────────
        if state.is_expanded(&entry.session_id) {
            if row < max_rows {
                let child_y = area.y + row as u16;
                let workspace_line = Line::from(vec![
                    Span::styled("  ", theme.muted),
                    Span::styled(&entry.workspace, theme.muted),
                ]);
                set_line_scrolled(buf, area, child_y, &workspace_line, hs, theme.muted);
                if let Some(style) = highlight {
                    apply_highlight(buf, area, child_y, style);
                }
                row += 1;
            }
            if row < max_rows && !entry.languages.is_empty() {
                let child_y = area.y + row as u16;
                let lang_line = Line::from(vec![
                    Span::styled("  ", theme.muted),
                    Span::styled(entry.languages.join(", "), theme.accent),
                ]);
                set_line_scrolled(buf, area, child_y, &lang_line, hs, theme.muted);
                if let Some(style) = highlight {
                    apply_highlight(buf, area, child_y, style);
                }
                row += 1;
            }
        }
    }

    hits
}

/// Render server entries into the given area (inside a `Block` frame).
///
/// No header text — the `Block` title replaces it. No vertical separator.
/// Dead servers (accumulated during this TUI session) render below a
/// thematic break, dimmed with "dead" state badge.
/// Returns a mapping from terminal row to server index for mouse clicks.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "render loop with dead-server thematic break; terminal coordinates are always small"
)]
pub fn render_servers(
    state: &SidebarState,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    focused: bool,
) -> Vec<(u16, usize)> {
    let mut hits: Vec<(u16, usize)> = Vec::new();

    if area.width == 0 || area.height == 0 {
        return hits;
    }

    let max_rows = area.height as usize;
    let mut row: usize = 0;
    let has_server_filter = state.has_server_filter();

    // Clamp hscroll so the user can't scroll past the longest line.
    let max_content: u16 = state
        .servers
        .iter()
        .map(|s| {
            let header = if s.root.is_empty() {
                s.name.width() + 2 + s.state.width()
            } else {
                s.name.width() + 1 + 1 + s.root.width() + 1 + 2 + s.state.width()
            };
            let progress = s.progress_line.as_ref().map_or(0, |p| 2 + p.width());
            let msg = s.server_message.as_ref().map_or(0, |m| 2 + m.width());
            header.max(progress).max(msg) as u16
        })
        .max()
        .unwrap_or(0);
    let hs = state
        .server_hscroll
        .min(max_content.saturating_sub(area.width));

    for (si, server) in state.servers.iter().enumerate() {
        if row >= max_rows {
            break;
        }

        let is_cursor = focused && si == state.server_cursor;
        let in_visual = focused
            && state
                .server_visual_range()
                .is_some_and(|(s, e)| si >= s && si <= e);

        let is_bright =
            !has_server_filter || state.is_server_selected(&server.name, &server.scope_root);
        let name_style = if is_bright { theme.text } else { theme.muted };
        let state_style = if is_bright {
            lifecycle_style(theme, &server.state)
        } else {
            theme.muted
        };

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
        set_line_scrolled(buf, area, y, &line, hs, theme.muted);
        hits.push((y, si));

        let highlight = if is_cursor {
            Some(&theme.selection)
        } else if in_visual {
            Some(&theme.visual_selection)
        } else {
            None
        };

        if let Some(style) = highlight {
            apply_highlight(buf, area, y, style);
        }
        row += 1;

        // ── Child lines: progress, then server message ─────
        if let Some(ref progress) = server.progress_line
            && row < max_rows
        {
            let child_y = area.y + row as u16;
            let child = Line::from(vec![
                Span::styled("  ", theme.muted),
                Span::styled(progress.clone(), theme.accent),
            ]);
            set_line_scrolled(buf, area, child_y, &child, hs, theme.muted);
            if let Some(style) = highlight {
                apply_highlight(buf, area, child_y, style);
            }
            row += 1;
        }
        if let Some(ref msg) = server.server_message
            && row < max_rows
        {
            let child_y = area.y + row as u16;
            let child = Line::from(vec![
                Span::styled("  ", theme.muted),
                Span::styled(msg.clone(), theme.muted),
            ]);
            set_line_scrolled(buf, area, child_y, &child, hs, theme.muted);
            if let Some(style) = highlight {
                apply_highlight(buf, area, child_y, style);
            }
            row += 1;
        }
    }

    // ── Dead servers below thematic break ──��───────────────────────
    if !state.dead_servers.is_empty() && row < max_rows {
        // Render separator line.
        let sep: String = "\u{2500}".repeat(area.width as usize);
        let sep_line = Line::from(Span::styled(sep, theme.muted));
        buf.set_line(area.x, area.y + row as u16, &sep_line, area.width);
        row += 1;

        for dead in &state.dead_servers {
            if row >= max_rows {
                break;
            }
            let line = if dead.root.is_empty() {
                Line::from(vec![
                    Span::styled(&dead.name, theme.muted),
                    Span::raw("  "),
                    Span::styled("dead", theme.muted),
                ])
            } else {
                Line::from(vec![
                    Span::styled(&dead.name, theme.muted),
                    Span::raw(" "),
                    Span::styled("(", theme.muted),
                    Span::styled(&dead.root, theme.muted),
                    Span::styled(")", theme.muted),
                    Span::raw("  "),
                    Span::styled("dead", theme.muted),
                ])
            };
            buf.set_line(area.x, area.y + row as u16, &line, area.width);
            row += 1;
        }
    }

    hits
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

    /// Build a `SessionData` from the old `(id, client_name, workspace)` tuple.
    fn sd(id: &str, client_name: Option<&str>, workspace: &str) -> SessionData {
        SessionData {
            id: id.to_string(),
            client_name: client_name.map(str::to_string),
            workspace: workspace.to_string(),
            languages: Vec::new(),
        }
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
    fn refresh_adds_and_removes_sessions() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();

        // Add two sessions.
        state.refresh(
            vec![
                sd("s1", Some("claude"), "/tmp/A"),
                sd("s2", Some("gemini"), "/tmp/B"),
            ],
            &mut badges,
        );
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0].badge, "00");
        assert_eq!(state.entries[1].badge, "01");

        // Remove s1, add s3.
        state.refresh(
            vec![
                sd("s2", Some("gemini"), "/tmp/B"),
                sd("s3", Some("claude"), "/tmp/C"),
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

        state.refresh(vec![sd("s1", None, "/tmp/A")], &mut badges);

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
                sd("s1", None, "/tmp/A"),
                sd("s2", None, "/tmp/B"),
                sd("s3", None, "/tmp/C"),
            ],
            &mut badges,
        );
        state.cursor = 2;

        // Remove two sessions — cursor should clamp.
        state.refresh(vec![sd("s1", None, "/tmp/A")], &mut badges);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn cursor_navigation() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                sd("s1", None, "/tmp/A"),
                sd("s2", None, "/tmp/B"),
                sd("s3", None, "/tmp/C"),
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
                sd("s1", None, "/tmp/A"),
                sd("s2", None, "/tmp/B"),
                sd("s3", None, "/tmp/C"),
                sd("s4", None, "/tmp/D"),
                sd("s5", None, "/tmp/E"),
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
    fn render_sessions_shows_entries() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                sd("s1", Some("claude"), "/Projects/Catenary"),
                sd("s2", Some("gemini"), "/Projects/OmniDSP"),
            ],
            &mut badges,
        );

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(25, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sessions(&state, area, frame.buffer_mut(), &theme, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("00"), "should show badge 00: {content}");
        assert!(content.contains("01"), "should show badge 01: {content}");
        assert!(content.contains("claude"), "should show host: {content}");
        assert!(content.contains("Catenary/"), "should show root: {content}");
    }

    #[test]
    fn render_sessions_zero_area() {
        let state = SidebarState::new();
        let theme = crate::tui::theme::Theme::new();
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(area);
        let hits = render_sessions(&state, area, &mut buf, &theme, true);
        assert!(hits.is_empty(), "zero-area should produce no hits");
    }

    // ── Selection tests ──────────────────────────────────────────────

    #[test]
    fn toggle_selected_flips_state() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![sd("s1", None, "/tmp/A"), sd("s2", None, "/tmp/B")],
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
                sd("s1", None, "/tmp/A"),
                sd("s2", None, "/tmp/B"),
                sd("s3", None, "/tmp/C"),
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
            vec![sd("s1", None, "/tmp/A"), sd("s2", None, "/tmp/B")],
            &mut badges,
        );

        // Select both.
        state.cursor = 0;
        state.toggle_selected();
        state.cursor = 1;
        state.toggle_selected();
        assert!(state.has_filter());

        // s2 disconnects.
        state.refresh(vec![sd("s1", None, "/tmp/A")], &mut badges);

        // s2 selection should be cleared; s1 remains.
        assert!(state.has_filter());
        assert!(state.is_selected("s1"));
        assert!(!state.is_selected("s2"));
    }

    #[test]
    fn refresh_clears_filter_when_all_selected_disconnect() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/tmp/A")], &mut badges);

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
    fn render_sessions_selected_bright_unselected_dim() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Modifier;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                sd("s1", Some("claude"), "/tmp/A"),
                sd("s2", Some("gemini"), "/tmp/B"),
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
                render_sessions(&state, area, frame.buffer_mut(), &theme, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // Entry layout: "XX host Root/" where root starts at
        // badge(2) + space(1) + host(6) + space(1) = 10.
        let root_col = 10;

        // Row 0 (s1, selected): root text should NOT be dim.
        let s1_root_cell = &buf[(root_col, 0)];
        assert!(
            !s1_root_cell.modifier.contains(Modifier::DIM),
            "selected entry root should be bright, got modifiers: {:?}",
            s1_root_cell.modifier
        );

        // Row 1 (s2, unselected): root text should be dim.
        let s2_root_cell = &buf[(root_col, 1)];
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
            vec![sd("s1", None, "/tmp/A"), sd("s2", None, "/tmp/B")],
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
    fn render_servers_shows_per_instance_servers() {
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
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_servers(&state, area, frame.buffer_mut(), &theme, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

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
    }

    // ── Progress / server message extraction tests ────────────────

    fn make_noise(
        server: &str,
        scope_root: &str,
        progress_title: Option<&str>,
        progress_pct: Option<u32>,
        last_message: Option<&str>,
    ) -> ServerNoiseRow {
        ServerNoiseRow {
            server: server.to_string(),
            scope_root: scope_root.to_string(),
            progress_title: progress_title.map(str::to_string),
            progress_pct,
            last_message: last_message.map(str::to_string),
        }
    }

    #[test]
    fn extract_progress_line_begin() {
        let noise = vec![make_noise(
            "rust-analyzer",
            "/A",
            Some("Indexing"),
            Some(47),
            None,
        )];
        let line = extract_progress_line(&noise, "rust-analyzer", "/A");
        assert_eq!(line.as_deref(), Some("Indexing… 47%"));
    }

    #[test]
    fn extract_progress_line_no_progress_returns_none() {
        let noise = vec![make_noise("rust-analyzer", "/A", None, None, None)];
        assert!(extract_progress_line(&noise, "rust-analyzer", "/A").is_none());
    }

    #[test]
    fn extract_progress_line_wrong_server() {
        let noise = vec![make_noise("lua-ls", "/A", Some("Loading"), None, None)];
        assert!(extract_progress_line(&noise, "rust-analyzer", "/A").is_none());
    }

    #[test]
    fn extract_server_message_log() {
        let noise = vec![make_noise(
            "rust-analyzer",
            "/A",
            None,
            None,
            Some("Fetching crate data"),
        )];
        let msg = extract_server_message(&noise, "rust-analyzer", "/A");
        assert_eq!(msg.as_deref(), Some("Fetching crate data"));
    }

    #[test]
    fn servers_need_refresh_detects_noise_change() {
        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "busy")];
        let noise = vec![make_noise(
            "rust-analyzer",
            "/A",
            Some("Indexing"),
            Some(10),
            None,
        )];
        state.refresh_servers(&rows, &noise);

        // Same noise — no refresh needed.
        assert!(!state.servers_need_refresh(&rows, &noise));

        // Changed percentage — refresh needed.
        let noise2 = vec![make_noise(
            "rust-analyzer",
            "/A",
            Some("Indexing"),
            Some(50),
            None,
        )];
        assert!(state.servers_need_refresh(&rows, &noise2));
    }

    #[test]
    fn render_servers_shows_progress_child() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "busy")];
        let noise = vec![make_noise(
            "rust-analyzer",
            "/A",
            Some("Indexing"),
            Some(47),
            None,
        )];
        state.refresh_servers(&rows, &noise);

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_servers(&state, area, frame.buffer_mut(), &theme, false);
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

    #[test]
    fn progress_scoped_by_instance() {
        let mut state = SidebarState::new();
        let rows = vec![
            make_server_row("rust-analyzer", "/A", "busy"),
            make_server_row("rust-analyzer", "/B", "busy"),
        ];
        let noise = vec![
            make_noise("rust-analyzer", "/A", Some("Indexing"), Some(20), None),
            make_noise("rust-analyzer", "/B", Some("Loading"), Some(80), None),
        ];
        state.refresh_servers(&rows, &noise);

        assert_eq!(state.servers.len(), 2);
        assert_eq!(
            state.servers[0].progress_line.as_deref(),
            Some("Indexing… 20%")
        );
        assert_eq!(
            state.servers[1].progress_line.as_deref(),
            Some("Loading… 80%")
        );
    }

    #[test]
    fn server_message_scoped_by_instance() {
        let mut state = SidebarState::new();
        let rows = vec![
            make_server_row("rust-analyzer", "/A", "ready"),
            make_server_row("rust-analyzer", "/B", "ready"),
        ];
        let noise = vec![
            make_noise(
                "rust-analyzer",
                "/A",
                None,
                None,
                Some("Workspace A loaded"),
            ),
            make_noise(
                "rust-analyzer",
                "/B",
                None,
                None,
                Some("Workspace B loaded"),
            ),
        ];
        state.refresh_servers(&rows, &noise);

        assert_eq!(
            state.servers[0].server_message.as_deref(),
            Some("Workspace A loaded")
        );
        assert_eq!(
            state.servers[1].server_message.as_deref(),
            Some("Workspace B loaded")
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
    fn render_servers_selected_bright_unselected_dim() {
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
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_servers(&state, area, frame.buffer_mut(), &theme, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // rust-analyzer = row 0, lua-ls = row 1
        let ra_cell = &buf[(0, 0)]; // "r" in "rust-analyzer"
        let lua_cell = &buf[(0, 1)]; // "l" in "lua-ls"

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

    // ── Dead server tests ───────────────────────────────────────────

    #[test]
    fn dead_server_accumulated_on_disappearance() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
            ],
            &[],
        );
        assert!(state.dead_servers.is_empty());

        // lua-ls disappears.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);

        assert_eq!(state.dead_servers.len(), 1);
        assert_eq!(state.dead_servers[0].name, "lua-ls");
        assert_eq!(state.dead_servers[0].scope_root, "/B");
        assert_eq!(state.dead_servers[0].state, "dead");
    }

    #[test]
    fn dead_server_not_duplicated() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
            ],
            &[],
        );

        // lua-ls disappears.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert_eq!(state.dead_servers.len(), 1);

        // Refresh again with same live set — dead list shouldn't grow.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert_eq!(state.dead_servers.len(), 1);
    }

    #[test]
    fn dead_server_removed_on_reappearance() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
            ],
            &[],
        );

        // lua-ls dies.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert_eq!(state.dead_servers.len(), 1);

        // lua-ls comes back (restarted).
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "initializing"),
            ],
            &[],
        );
        assert!(
            state.dead_servers.is_empty(),
            "reappeared server should be removed from dead list"
        );
    }

    #[test]
    fn no_dead_servers_on_initial_refresh() {
        let mut state = SidebarState::new();
        // First refresh — nothing should be considered dead.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert!(state.dead_servers.is_empty());
    }

    #[test]
    fn render_dead_servers_below_separator() {
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

        // lua-ls dies.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_servers(&state, area, frame.buffer_mut(), &theme, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Live server on row 0.
        assert!(
            content.contains("rust-analyzer"),
            "should show live server: {content}"
        );
        // Separator (box drawing horizontal).
        assert!(
            content.contains('\u{2500}'),
            "should show separator: {content}"
        );
        // Dead server below separator.
        assert!(
            content.contains("lua-ls"),
            "should show dead server: {content}"
        );
        assert!(
            content.contains("dead"),
            "should show dead badge: {content}"
        );

        // Dead server text should be dimmed.
        // Row 0 = live server, row 1 = separator, row 2 = dead server.
        let dead_cell = &buf[(0, 2)];
        assert!(
            dead_cell.modifier.contains(Modifier::DIM),
            "dead server should be dimmed, got: {:?}",
            dead_cell.modifier
        );
    }

    #[test]
    fn multiple_dead_servers_accumulate() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/B", "ready"),
                make_server_row("pyright", "/C", "ready"),
            ],
            &[],
        );

        // lua-ls dies first.
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("pyright", "/C", "ready"),
            ],
            &[],
        );
        assert_eq!(state.dead_servers.len(), 1);
        assert_eq!(state.dead_servers[0].name, "lua-ls");

        // Then pyright dies too.
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert_eq!(state.dead_servers.len(), 2);
        assert_eq!(state.dead_servers[0].name, "lua-ls");
        assert_eq!(state.dead_servers[1].name, "pyright");
    }

    // ── Horizontal scroll tests ──────────────────────────────────────

    #[test]
    fn hscroll_line_zero_offset_preserves_content() {
        let line = Line::from(vec![Span::raw("abc"), Span::raw(" "), Span::raw("def")]);
        let result = hscroll_line(&line, 0);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "abc def");
    }

    #[test]
    fn hscroll_line_partial_span() {
        let line = Line::from(vec![Span::raw("abcdef")]);
        let result = hscroll_line(&line, 3);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "def");
    }

    #[test]
    fn hscroll_line_across_span_boundary() {
        let line = Line::from(vec![Span::raw("ab"), Span::raw("cd"), Span::raw("ef")]);
        let result = hscroll_line(&line, 3);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "def");
    }

    #[test]
    fn hscroll_line_exact_span_boundary() {
        let line = Line::from(vec![Span::raw("ab"), Span::raw("cd")]);
        let result = hscroll_line(&line, 2);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "cd");
    }

    #[test]
    fn hscroll_line_past_end_is_empty() {
        let line = Line::from(vec![Span::raw("abc")]);
        let result = hscroll_line(&line, 10);
        assert!(result.spans.is_empty());
    }

    #[test]
    fn hscroll_sessions_left_clamps_at_zero() {
        let mut state = SidebarState::new();
        state.session_hscroll = 2;
        state.hscroll_sessions_left();
        assert_eq!(state.session_hscroll, 0, "should clamp at zero");
        state.hscroll_sessions_left();
        assert_eq!(state.session_hscroll, 0, "should stay at zero");
    }

    #[test]
    fn hscroll_sessions_right_increments() {
        let mut state = SidebarState::new();
        state.hscroll_sessions_right();
        assert_eq!(state.session_hscroll, HSCROLL_STEP);
        state.hscroll_sessions_right();
        assert_eq!(state.session_hscroll, HSCROLL_STEP * 2);
    }

    #[test]
    fn hscroll_servers_independent_from_sessions() {
        let mut state = SidebarState::new();
        state.hscroll_sessions_right();
        state.hscroll_servers_right();
        state.hscroll_servers_right();
        assert_eq!(state.session_hscroll, HSCROLL_STEP);
        assert_eq!(state.server_hscroll, HSCROLL_STEP * 2);
    }

    #[test]
    fn render_sessions_hscrolled_shows_indicator() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![sd("s1", Some("claude-code"), "/Projects/Catenary")],
            &mut badges,
        );
        state.session_hscroll = 4;

        let theme = crate::tui::theme::Theme::new();
        // Content is 19 cols ("00 claude Catenary/"); narrow to 15 so hscroll is effective.
        let backend = TestBackend::new(15, 3);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sessions(&state, area, frame.buffer_mut(), &theme, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains('\u{2026}'),
            "should show scroll indicator: {content}"
        );
    }

    #[test]
    fn render_sessions_no_indicator_at_zero() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![sd("s1", Some("claude-code"), "/Projects/Catenary")],
            &mut badges,
        );

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(25, 3);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sessions(&state, area, frame.buffer_mut(), &theme, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("00"),
            "should show badge without indicator: {content}"
        );
        assert!(
            !content.contains('\u{2026}'),
            "should not show indicator at hscroll 0: {content}"
        );
    }

    #[test]
    fn render_servers_hscrolled_shows_indicator() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        state.refresh_servers(
            &[make_server_row(
                "rust-analyzer",
                "/home/user/Catenary",
                "ready",
            )],
            &[],
        );
        state.server_hscroll = 4;

        let theme = crate::tui::theme::Theme::new();
        // Content is ~31 cols; narrow to 25 so hscroll is effective.
        let backend = TestBackend::new(25, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_servers(&state, area, frame.buffer_mut(), &theme, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains('\u{2026}'),
            "should show scroll indicator: {content}"
        );
    }

    #[test]
    fn hscroll_clamped_when_content_fits() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        // Content: "00 claude A/" = 12 cols.
        state.refresh(vec![sd("s1", Some("claude"), "/A")], &mut badges);
        // Request a large hscroll, but content fits in 25 cols.
        state.session_hscroll = 20;

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(25, 3);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_sessions(&state, area, frame.buffer_mut(), &theme, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Clamped to 0 — no indicator, full content visible.
        assert!(
            !content.contains('\u{2026}'),
            "should not show indicator when content fits: {content}"
        );
        assert!(content.contains("00"), "should show badge: {content}");
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
