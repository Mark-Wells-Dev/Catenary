// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Sidebar widget: unified workspace panel grouping connections and
//! servers by workspace root.
//!
//! Replaces the former separate Connections and Servers panels with a
//! single "Workspaces" panel. Each workspace root is a top-level entry
//! that expands to show connected sessions and server instances.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::data::{ServerNoiseRow, ServerStatusRow};
use super::stream::HexBadgeMap;
use super::theme::Theme;

/// Number of columns to scroll per horizontal scroll step.
const HSCROLL_STEP: u16 = 4;

// ── Session entry ────────────────────────────────────────────────────

/// A session entry (internal data, not directly rendered).
pub struct SessionEntry {
    /// Session ID (database key).
    pub session_id: String,
    /// Two-character hex badge.
    pub badge: String,
    /// Host format label ("claude-code", "gemini-cli", etc.).
    pub host: String,
    /// Primary workspace root directory name with trailing slash.
    pub root: String,
    /// Full workspace path(s), comma-separated.
    pub workspace: String,
    /// Active language servers.
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

/// A per-instance server entry.
pub struct ServerEntry {
    /// Server binary name (config key, e.g., "rust-analyzer").
    pub name: String,
    /// Full scope root path (e.g., "/home/user/project").
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

// ── Workspace types ─────────────────────────────────────────────────

/// Server instance identity: `(server_name, scope_root)`.
pub type ServerInstanceKey = (String, String);

/// A workspace root grouping connections and servers.
pub struct WorkspaceEntry {
    /// Full root path (grouping key).
    pub root_path: String,
    /// Display name (last path segment with trailing `/`).
    pub root_name: String,
    /// Connected sessions grouped by host label.
    pub connections: Vec<ConnectionGroup>,
    /// Indices into `SidebarState::servers` for this root.
    pub server_indices: Vec<usize>,
    /// Indices into `SidebarState::dead_servers` for this root.
    pub dead_server_indices: Vec<usize>,
}

/// Sessions grouped by host label within a workspace.
pub struct ConnectionGroup {
    /// Host label (e.g., "claude-code", "gemini-cli").
    pub host: String,
    /// Number of sessions with this host label.
    pub count: usize,
    /// Session IDs belonging to this group.
    pub session_ids: Vec<String>,
}

/// Row type in the flattened workspace panel. Each variant carries
/// indices for lookup into `SidebarState` fields.
#[derive(Debug, Clone, Copy)]
pub enum WorkspaceRow {
    /// Workspace root entry. `usize` = workspace index.
    Root(usize),
    /// "Connections:" section header. `usize` = workspace index.
    ConnectionHeader(usize),
    /// Connection group. `(workspace_idx, connection_group_idx)`.
    Connection(usize, usize),
    /// "Servers:" section header. `usize` = workspace index.
    ServerHeader(usize),
    /// Live server entry. `(workspace_idx, global_server_idx)`.
    Server(usize, usize),
    /// Progress child of a server. `global_server_idx`.
    ServerProgress(usize),
    /// Message child of a server. `global_server_idx`.
    ServerMessage(usize),
    /// Dead server entry. `(workspace_idx, global_dead_server_idx)`.
    DeadServer(usize, usize),
}

// ── Sidebar state ────────────────────────────────────────────────────

/// Sidebar state: unified workspace panel with connections and servers
/// grouped by workspace root.
pub struct SidebarState {
    // ── Raw data (refreshed from DB) ────────────────────────────
    /// Alive sessions in display order.
    pub entries: Vec<SessionEntry>,
    /// Active server entries.
    pub servers: Vec<ServerEntry>,
    /// Servers observed to die during this TUI session.
    pub dead_servers: Vec<ServerEntry>,

    // ── Workspace model (derived from raw data) ─────────────────
    /// Workspace entries grouped by root path.
    pub workspaces: Vec<WorkspaceEntry>,
    /// Flattened rows for cursor navigation and rendering.
    pub workspace_rows: Vec<WorkspaceRow>,

    // ── Cursor and scroll ───────────────────────────────────────
    /// Cursor position in the flattened workspace rows.
    pub cursor: usize,
    /// First visible row (scroll offset).
    pub scroll_offset: usize,

    // ── Change detection ────────────────────────────────────────
    /// Session IDs from the last refresh.
    last_ids: Vec<String>,
    /// Server fingerprints from the last refresh.
    last_server_names: Vec<String>,

    // ── Selection / filtering ───────────────────────────────────
    /// Selected workspace root paths (for stream filtering).
    /// Empty = show all.
    selected_roots: HashSet<String>,
    /// Selected server instances `(name, scope_root)` (for stream filtering).
    /// Empty = show all.
    selected_servers: HashSet<ServerInstanceKey>,

    // ── Expansion ───────────────────────────────────────────────
    /// Workspace roots with expanded detail.
    expanded_roots: HashSet<String>,

    // ── Horizontal scroll ───────────────────────────────────────
    /// Horizontal scroll offset (in columns).
    hscroll: u16,

    // ── Visual selection ────────────────────────────────────────
    /// Visual selection anchor (row index in `workspace_rows`).
    visual_anchor: Option<usize>,
}

impl SidebarState {
    /// Create an empty sidebar.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            servers: Vec::new(),
            dead_servers: Vec::new(),
            workspaces: Vec::new(),
            workspace_rows: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            last_ids: Vec::new(),
            last_server_names: Vec::new(),
            selected_roots: HashSet::new(),
            selected_servers: HashSet::new(),
            expanded_roots: HashSet::new(),
            hscroll: 0,
            visual_anchor: None,
        }
    }

    // ── Session refresh ─────────────────────────────────────────

    /// Check whether the alive session IDs have changed since the last
    /// refresh.
    #[must_use]
    pub fn needs_refresh(&self, current_ids: &[String]) -> bool {
        self.last_ids != current_ids
    }

    /// Update the sidebar with fresh session data.
    pub fn refresh(&mut self, sessions: Vec<SessionData>, badges: &mut HexBadgeMap) {
        let new_ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        for entry in &self.entries {
            if !new_ids.contains(entry.session_id.as_str()) {
                badges.release(&entry.session_id);
            }
        }
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
        self.rebuild_workspaces();
    }

    // ── Server refresh ──────────────────────────────────────────

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
        if !self.servers.is_empty() {
            for server in &self.servers {
                let key = (server.name.as_str(), server.scope_root.as_str());
                if !new_keys.contains(&key) {
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

        self.rebuild_workspaces();
    }

    // ── Workspace building ──────────────────────────────────────

    /// Rebuild workspace entries and flattened rows from session and
    /// server data. Called after every `refresh()` or `refresh_servers()`.
    fn rebuild_workspaces(&mut self) {
        self.build_workspaces();
        self.compute_workspace_rows();

        // Prune stale root selections.
        let root_set: HashSet<&str> = self
            .workspaces
            .iter()
            .map(|w| w.root_path.as_str())
            .collect();
        self.selected_roots
            .retain(|r| root_set.contains(r.as_str()));
        self.expanded_roots
            .retain(|r| root_set.contains(r.as_str()));

        // Clamp cursor.
        let max = self.workspace_rows.len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
        if self.scroll_offset > self.cursor {
            self.scroll_offset = self.cursor;
        }
    }

    /// Group sessions and servers by workspace root path.
    fn build_workspaces(&mut self) {
        // Collect unique root paths from sessions and servers.
        let mut roots: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for entry in &self.entries {
            for part in entry.workspace.split(',') {
                let path = part.trim().to_string();
                if !path.is_empty() && seen.insert(path.clone()) {
                    roots.push(path);
                }
            }
        }
        for server in &self.servers {
            if !server.scope_root.is_empty() && seen.insert(server.scope_root.clone()) {
                roots.push(server.scope_root.clone());
            }
        }
        for dead in &self.dead_servers {
            if !dead.scope_root.is_empty() && seen.insert(dead.scope_root.clone()) {
                roots.push(dead.scope_root.clone());
            }
        }

        // Sort by display name for stable ordering.
        roots.sort_by_key(|a| server_root_name(a));

        self.workspaces = roots
            .into_iter()
            .map(|root_path| {
                let root_name = server_root_name(&root_path);

                // Group sessions by host label.
                let mut host_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for entry in &self.entries {
                    let has_root = entry.workspace.split(',').any(|p| p.trim() == root_path);
                    if has_root {
                        host_map
                            .entry(entry.host.clone())
                            .or_default()
                            .push(entry.session_id.clone());
                    }
                }
                let connections: Vec<ConnectionGroup> = host_map
                    .into_iter()
                    .map(|(host, ids)| ConnectionGroup {
                        count: ids.len(),
                        host,
                        session_ids: ids,
                    })
                    .collect();

                // Indices into self.servers for this root.
                let server_indices: Vec<usize> = self
                    .servers
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.scope_root == root_path)
                    .map(|(i, _)| i)
                    .collect();

                // Indices into self.dead_servers for this root.
                let dead_server_indices: Vec<usize> = self
                    .dead_servers
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| d.scope_root == root_path)
                    .map(|(i, _)| i)
                    .collect();

                WorkspaceEntry {
                    root_path,
                    root_name,
                    connections,
                    server_indices,
                    dead_server_indices,
                }
            })
            .collect();
    }

    /// Flatten workspace entries into cursor-navigable rows based on
    /// expansion state.
    fn compute_workspace_rows(&mut self) {
        self.workspace_rows.clear();

        for (ws_idx, ws) in self.workspaces.iter().enumerate() {
            self.workspace_rows.push(WorkspaceRow::Root(ws_idx));

            if !self.expanded_roots.contains(&ws.root_path) {
                continue;
            }

            // Connections section (only if there are connections).
            if !ws.connections.is_empty() {
                self.workspace_rows
                    .push(WorkspaceRow::ConnectionHeader(ws_idx));
                for (ci, _) in ws.connections.iter().enumerate() {
                    self.workspace_rows
                        .push(WorkspaceRow::Connection(ws_idx, ci));
                }
            }

            // Servers section (only if there are live or dead servers).
            if !ws.server_indices.is_empty() || !ws.dead_server_indices.is_empty() {
                self.workspace_rows.push(WorkspaceRow::ServerHeader(ws_idx));
                for &srv_idx in &ws.server_indices {
                    self.workspace_rows
                        .push(WorkspaceRow::Server(ws_idx, srv_idx));
                    if self.servers[srv_idx].progress_line.is_some() {
                        self.workspace_rows
                            .push(WorkspaceRow::ServerProgress(srv_idx));
                    }
                    if self.servers[srv_idx].server_message.is_some() {
                        self.workspace_rows
                            .push(WorkspaceRow::ServerMessage(srv_idx));
                    }
                }
                for &dead_idx in &ws.dead_server_indices {
                    self.workspace_rows
                        .push(WorkspaceRow::DeadServer(ws_idx, dead_idx));
                }
            }
        }
    }

    // ── Cursor navigation ───────────────────────────────────────

    /// Move cursor up by `n` rows, scrolling if needed.
    pub const fn cursor_up(&mut self, n: usize, visible: usize) {
        let _ = visible;
        self.cursor = self.cursor.saturating_sub(n);
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        }
    }

    /// Move cursor down by `n` rows, scrolling if needed.
    pub fn cursor_down(&mut self, n: usize, visible: usize) {
        let total = self.workspace_rows.len();
        if total == 0 {
            return;
        }
        let max = total.saturating_sub(1);
        self.cursor = (self.cursor + n).min(max);
        if visible > 0 && self.cursor >= self.scroll_offset + visible {
            self.scroll_offset = self.cursor + 1 - visible;
        }
    }

    // ── Horizontal scroll ───────────────────────────────────────

    /// Scroll left by one step.
    pub const fn hscroll_left(&mut self) {
        self.hscroll = self.hscroll.saturating_sub(HSCROLL_STEP);
    }

    /// Scroll right by one step.
    pub const fn hscroll_right(&mut self) {
        self.hscroll = self.hscroll.saturating_add(HSCROLL_STEP);
    }

    // ── Expansion ───────────────────────────────────────────────

    /// Toggle expansion on the workspace root at the current cursor.
    pub fn toggle_expanded(&mut self) {
        let Some(row) = self.workspace_rows.get(self.cursor) else {
            return;
        };
        if let WorkspaceRow::Root(ws_idx) = row {
            let path = &self.workspaces[*ws_idx].root_path;
            if self.expanded_roots.contains(path) {
                self.expanded_roots.remove(path);
            } else {
                self.expanded_roots.insert(path.clone());
            }
            self.compute_workspace_rows();
            let max = self.workspace_rows.len().saturating_sub(1);
            self.cursor = self.cursor.min(max);
        }
    }

    /// Whether a workspace root is expanded.
    #[must_use]
    pub fn is_root_expanded(&self, root_path: &str) -> bool {
        self.expanded_roots.contains(root_path)
    }

    // ── Selection / filtering ───────────────────────────────────

    /// Toggle selection on the item at the current cursor.
    ///
    /// - Root rows: toggles workspace root selection.
    /// - Server rows: toggles per-instance server selection.
    /// - Other rows: no-op.
    ///
    /// Returns `true` if a filter changed.
    pub fn toggle_selected(&mut self) -> bool {
        let Some(row) = self.workspace_rows.get(self.cursor).copied() else {
            return false;
        };
        match row {
            WorkspaceRow::Root(ws_idx) => {
                let path = self.workspaces[ws_idx].root_path.clone();
                if self.selected_roots.contains(&path) {
                    self.selected_roots.remove(&path);
                } else {
                    self.selected_roots.insert(path);
                }
                true
            }
            WorkspaceRow::Server(_, srv_idx) => {
                let srv = &self.servers[srv_idx];
                let key: ServerInstanceKey = (srv.name.clone(), srv.scope_root.clone());
                if self.selected_servers.contains(&key) {
                    self.selected_servers.remove(&key);
                } else {
                    self.selected_servers.insert(key);
                }
                true
            }
            _ => false,
        }
    }

    /// Build the workspace root filter from selected roots.
    ///
    /// `None` = show all. `Some(set)` = filter stream entries by
    /// `scope_root` matching a selected workspace root path.
    #[must_use]
    pub fn root_filter(&self) -> Option<HashSet<String>> {
        if self.selected_roots.is_empty() {
            None
        } else {
            Some(self.selected_roots.clone())
        }
    }

    /// Return the active server filter.
    ///
    /// `None` = show all. `Some(set)` = show only scopes involving
    /// server instances in the set.
    #[must_use]
    pub fn server_filter(&self) -> Option<HashSet<ServerInstanceKey>> {
        if self.selected_servers.is_empty() {
            None
        } else {
            Some(self.selected_servers.clone())
        }
    }

    /// Whether any filter is active (roots or servers).
    #[must_use]
    pub fn has_filter(&self) -> bool {
        !self.selected_roots.is_empty() || !self.selected_servers.is_empty()
    }

    /// Whether a specific workspace root is selected.
    #[must_use]
    pub fn is_root_selected(&self, root_path: &str) -> bool {
        self.selected_roots.contains(root_path)
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

    // ── Server popup ────────────────────────────────────────────

    /// Return the server index at the current cursor, if the cursor
    /// is on a server row.
    #[must_use]
    pub fn cursor_server_index(&self) -> Option<usize> {
        match self.workspace_rows.get(self.cursor)? {
            WorkspaceRow::Server(_, srv_idx) => Some(*srv_idx),
            WorkspaceRow::ServerProgress(srv_idx) | WorkspaceRow::ServerMessage(srv_idx) => {
                Some(*srv_idx)
            }
            _ => None,
        }
    }

    // ── Visual selection ────────────────────────────────────────

    /// Enter visual selection mode, anchoring at the cursor.
    pub const fn start_visual(&mut self) {
        self.visual_anchor = Some(self.cursor);
    }

    /// Exit visual selection mode.
    pub const fn exit_visual(&mut self) {
        self.visual_anchor = None;
    }

    /// Whether visual selection is active.
    #[must_use]
    pub const fn in_visual(&self) -> bool {
        self.visual_anchor.is_some()
    }

    /// Inclusive range of the visual selection.
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

    /// Yank text for the visual selection (or current cursor row).
    #[must_use]
    pub fn yank_text(&self) -> Option<String> {
        let (start, end) = self.visual_range().unwrap_or((self.cursor, self.cursor));
        let lines: Vec<String> = (start..=end)
            .filter_map(|i| self.row_plain_text(i))
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    /// Plain text for a single workspace row (for yank).
    fn row_plain_text(&self, idx: usize) -> Option<String> {
        let row = self.workspace_rows.get(idx)?;
        match *row {
            WorkspaceRow::Root(ws_idx) => Some(self.workspaces[ws_idx].root_name.clone()),
            WorkspaceRow::ConnectionHeader(_) => Some("  Connections:".to_string()),
            WorkspaceRow::Connection(ws_idx, ci) => {
                let cg = &self.workspaces[ws_idx].connections[ci];
                if cg.count > 1 {
                    Some(format!("    {} ({})", cg.host, cg.count))
                } else {
                    Some(format!("    {}", cg.host))
                }
            }
            WorkspaceRow::ServerHeader(_) => Some("  Servers:".to_string()),
            WorkspaceRow::Server(_, srv_idx) => {
                let srv = &self.servers[srv_idx];
                Some(format!("    {}  {}", srv.name, srv.state))
            }
            WorkspaceRow::ServerProgress(srv_idx) => {
                let line = self.servers[srv_idx].progress_line.as_ref()?;
                Some(format!("      {line}"))
            }
            WorkspaceRow::ServerMessage(srv_idx) => {
                let msg = self.servers[srv_idx].server_message.as_ref()?;
                Some(format!("      {msg}"))
            }
            WorkspaceRow::DeadServer(_, dead_idx) => {
                let dead = &self.dead_servers[dead_idx];
                Some(format!("    {}  dead", dead.name))
            }
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

/// Extract the most recent server message from noise data.
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

/// Choose a style for a lifecycle state string.
fn lifecycle_style(theme: &Theme, state: &str) -> ratatui::style::Style {
    match state {
        "ready" => theme.text,
        "busy" => theme.accent,
        _ => theme.muted,
    }
}

/// Render the unified workspace panel.
///
/// Returns a mapping from terminal row to `workspace_rows` index for
/// mouse click dispatch.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "tree renderer with multiple row types; terminal coordinates are always small"
)]
pub fn render_workspaces(
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
    let has_root_filter = !state.selected_roots.is_empty();
    let has_server_filter = state.has_server_filter();
    let hs = state.hscroll;

    let visible_rows = state
        .workspace_rows
        .iter()
        .enumerate()
        .skip(state.scroll_offset);

    for (rendered, (row_idx, ws_row)) in visible_rows.enumerate() {
        if rendered >= max_rows {
            break;
        }
        let y = area.y + rendered as u16;
        let is_cursor = focused && row_idx == state.cursor;
        let in_visual = focused
            && state
                .visual_range()
                .is_some_and(|(s, e)| row_idx >= s && row_idx <= e);

        let line = match *ws_row {
            WorkspaceRow::Root(ws_idx) => {
                let ws = &state.workspaces[ws_idx];
                let is_bright = !has_root_filter || state.is_root_selected(&ws.root_path);
                let style = if is_bright { theme.text } else { theme.muted };
                let arrow = if state.is_root_expanded(&ws.root_path) {
                    "\u{25bc} "
                } else {
                    "\u{25b6} "
                };
                Line::from(vec![
                    Span::styled(arrow, theme.muted),
                    Span::styled(&ws.root_name, style),
                ])
            }
            WorkspaceRow::ConnectionHeader(_) => Line::from(vec![
                Span::styled("  ", theme.muted),
                Span::styled("Connections:", theme.muted),
            ]),
            WorkspaceRow::Connection(ws_idx, ci) => {
                let cg = &state.workspaces[ws_idx].connections[ci];
                let label = if cg.count > 1 {
                    format!("{} ({})", cg.host, cg.count)
                } else {
                    cg.host.clone()
                };
                Line::from(vec![
                    Span::styled("    ", theme.muted),
                    Span::styled(label, theme.muted),
                ])
            }
            WorkspaceRow::ServerHeader(_) => Line::from(vec![
                Span::styled("  ", theme.muted),
                Span::styled("Servers:", theme.muted),
            ]),
            WorkspaceRow::Server(ws_idx, srv_idx) => {
                let srv = &state.servers[srv_idx];
                let ws_root = &state.workspaces[ws_idx].root_path;
                let is_bright_root = !has_root_filter || state.is_root_selected(ws_root);
                let is_bright_srv =
                    !has_server_filter || state.is_server_selected(&srv.name, &srv.scope_root);
                let is_bright = is_bright_root && is_bright_srv;
                let name_style = if is_bright { theme.text } else { theme.muted };
                let state_style = if is_bright {
                    lifecycle_style(theme, &srv.state)
                } else {
                    theme.muted
                };
                Line::from(vec![
                    Span::styled("    ", theme.muted),
                    Span::styled(&srv.name, name_style),
                    Span::raw("  "),
                    Span::styled(&srv.state, state_style),
                ])
            }
            WorkspaceRow::ServerProgress(srv_idx) => {
                let line_text = state.servers[srv_idx]
                    .progress_line
                    .as_deref()
                    .unwrap_or("");
                Line::from(vec![
                    Span::styled("      ", theme.muted),
                    Span::styled(line_text.to_string(), theme.accent),
                ])
            }
            WorkspaceRow::ServerMessage(srv_idx) => {
                let msg = state.servers[srv_idx]
                    .server_message
                    .as_deref()
                    .unwrap_or("");
                Line::from(vec![
                    Span::styled("      ", theme.muted),
                    Span::styled(msg.to_string(), theme.muted),
                ])
            }
            WorkspaceRow::DeadServer(_, dead_idx) => {
                let dead = &state.dead_servers[dead_idx];
                Line::from(vec![
                    Span::styled("    ", theme.muted),
                    Span::styled(&dead.name, theme.muted),
                    Span::raw("  "),
                    Span::styled("dead", theme.muted),
                ])
            }
        };

        set_line_scrolled(buf, area, y, &line, hs, theme.muted);
        hits.push((y, row_idx));

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
    }

    hits
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    fn sd(id: &str, client_name: Option<&str>, workspace: &str) -> SessionData {
        SessionData {
            id: id.to_string(),
            client_name: client_name.map(str::to_string),
            workspace: workspace.to_string(),
            languages: Vec::new(),
        }
    }

    fn make_server_row(server: &str, scope_root: &str, state: &str) -> ServerStatusRow {
        ServerStatusRow {
            language_id: "rust".to_string(),
            server: server.to_string(),
            scope_kind: "root".to_string(),
            scope_root: scope_root.to_string(),
            state: state.to_string(),
        }
    }

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

    // ── Label helpers ─────────────────────────────────────────────

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

    // ── Session refresh ─────────────────────────────────────────

    #[test]
    fn refresh_adds_and_removes_sessions() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();

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

        state.refresh(
            vec![
                sd("s2", Some("gemini"), "/tmp/B"),
                sd("s3", Some("claude"), "/tmp/C"),
            ],
            &mut badges,
        );
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0].badge, "01");
        assert_eq!(state.entries[1].badge, "00");
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

    // ── Workspace building ──────────────────────────────────────

    #[test]
    fn workspaces_group_by_root() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                sd("s1", Some("claude-code"), "/home/user/Catenary"),
                sd("s2", Some("gemini-cli"), "/home/user/Catenary"),
                sd("s3", Some("claude-code"), "/home/user/Lattice"),
            ],
            &mut badges,
        );
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/home/user/Catenary", "ready"),
                make_server_row("taplo", "/home/user/Catenary", "initializing"),
                make_server_row("rust-analyzer", "/home/user/Lattice", "ready"),
            ],
            &[],
        );

        assert_eq!(state.workspaces.len(), 2);

        let catenary = state
            .workspaces
            .iter()
            .find(|w| w.root_name == "Catenary/")
            .expect("should have Catenary workspace");
        assert_eq!(catenary.connections.len(), 2);
        assert_eq!(catenary.server_indices.len(), 2);

        let lattice = state
            .workspaces
            .iter()
            .find(|w| w.root_name == "Lattice/")
            .expect("should have Lattice workspace");
        assert_eq!(lattice.connections.len(), 1);
        assert_eq!(lattice.server_indices.len(), 1);
    }

    #[test]
    fn connections_group_by_host_with_count() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                sd("s1", Some("claude-code"), "/A"),
                sd("s2", Some("claude-code"), "/A"),
                sd("s3", Some("gemini-cli"), "/A"),
            ],
            &mut badges,
        );

        assert_eq!(state.workspaces.len(), 1);
        let ws = &state.workspaces[0];
        assert_eq!(ws.connections.len(), 2);

        let claude = ws
            .connections
            .iter()
            .find(|c| c.host == "claude-code")
            .expect("should have claude-code group");
        assert_eq!(claude.count, 2);
        assert_eq!(claude.session_ids.len(), 2);
    }

    #[test]
    fn workspace_rows_collapsed() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/A")], &mut badges);
        state.refresh_servers(&[make_server_row("ra", "/A", "ready")], &[]);

        assert_eq!(state.workspace_rows.len(), 1);
        assert!(matches!(state.workspace_rows[0], WorkspaceRow::Root(0)));
    }

    #[test]
    fn workspace_rows_expanded() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", Some("claude"), "/A")], &mut badges);
        state.refresh_servers(&[make_server_row("ra", "/A", "ready")], &[]);

        state.cursor = 0;
        state.toggle_expanded();

        assert_eq!(state.workspace_rows.len(), 5);
        assert!(matches!(state.workspace_rows[0], WorkspaceRow::Root(0)));
        assert!(matches!(
            state.workspace_rows[1],
            WorkspaceRow::ConnectionHeader(0)
        ));
        assert!(matches!(
            state.workspace_rows[2],
            WorkspaceRow::Connection(0, 0)
        ));
        assert!(matches!(
            state.workspace_rows[3],
            WorkspaceRow::ServerHeader(0)
        ));
        assert!(matches!(
            state.workspace_rows[4],
            WorkspaceRow::Server(0, 0)
        ));
    }

    #[test]
    fn workspace_rows_include_progress_and_message() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/A")], &mut badges);
        state.refresh_servers(
            &[make_server_row("ra", "/A", "busy")],
            &[make_noise(
                "ra",
                "/A",
                Some("Indexing"),
                Some(47),
                Some("msg"),
            )],
        );

        state.cursor = 0;
        state.toggle_expanded();

        assert_eq!(state.workspace_rows.len(), 7);
        assert!(matches!(
            state.workspace_rows[5],
            WorkspaceRow::ServerProgress(0)
        ));
        assert!(matches!(
            state.workspace_rows[6],
            WorkspaceRow::ServerMessage(0)
        ));
    }

    // ── Dead servers ────────────────────────────────────────────

    #[test]
    fn dead_server_accumulated_on_disappearance() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/A", "ready"),
            ],
            &[],
        );

        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);

        assert_eq!(state.dead_servers.len(), 1);
        assert_eq!(state.dead_servers[0].name, "lua-ls");
    }

    #[test]
    fn dead_server_not_duplicated() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/A", "ready"),
            ],
            &[],
        );

        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert_eq!(state.dead_servers.len(), 1);

        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert_eq!(state.dead_servers.len(), 1);
    }

    #[test]
    fn dead_server_removed_on_reappearance() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/A", "ready"),
            ],
            &[],
        );

        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert_eq!(state.dead_servers.len(), 1);

        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/A", "initializing"),
            ],
            &[],
        );
        assert!(state.dead_servers.is_empty());
    }

    #[test]
    fn no_dead_servers_on_initial_refresh() {
        let mut state = SidebarState::new();
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);
        assert!(state.dead_servers.is_empty());
    }

    #[test]
    fn dead_servers_appear_in_expanded_workspace() {
        let mut state = SidebarState::new();
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/A", "ready"),
            ],
            &[],
        );
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);

        state.cursor = 0;
        state.toggle_expanded();

        let has_dead = state
            .workspace_rows
            .iter()
            .any(|r| matches!(r, WorkspaceRow::DeadServer(_, _)));
        assert!(has_dead, "expanded workspace should show dead servers");
    }

    // ── Cursor navigation ───────────────────────────────────────

    #[test]
    fn cursor_navigation() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                sd("s1", None, "/A"),
                sd("s2", None, "/B"),
                sd("s3", None, "/C"),
            ],
            &mut badges,
        );

        assert_eq!(state.workspace_rows.len(), 3);

        state.cursor_down(1, 10);
        assert_eq!(state.cursor, 1);
        state.cursor_down(5, 10);
        assert_eq!(state.cursor, 2, "should clamp to last row");
        state.cursor_up(1, 10);
        assert_eq!(state.cursor, 1);
        state.cursor_up(5, 10);
        assert_eq!(state.cursor, 0, "should clamp to 0");
    }

    #[test]
    fn cursor_clamps_on_workspace_removal() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![
                sd("s1", None, "/A"),
                sd("s2", None, "/B"),
                sd("s3", None, "/C"),
            ],
            &mut badges,
        );
        assert_eq!(state.workspace_rows.len(), 3);
        state.cursor = 2;

        // Remove two sessions — only one workspace root remains.
        state.refresh(vec![sd("s1", None, "/A")], &mut badges);
        assert_eq!(state.workspace_rows.len(), 1);
        assert_eq!(state.cursor, 0, "cursor should clamp to last row");
    }

    #[test]
    fn toggle_collapse_from_root_row() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", Some("claude"), "/A")], &mut badges);
        state.refresh_servers(&[make_server_row("ra", "/A", "ready")], &[]);

        // Expand.
        state.cursor = 0;
        state.toggle_expanded();
        assert_eq!(state.workspace_rows.len(), 5);

        // Collapse (cursor must be on root row).
        state.cursor = 0;
        state.toggle_expanded();
        assert_eq!(state.workspace_rows.len(), 1);
        assert_eq!(state.cursor, 0);
    }

    // ── Selection / filtering ───────────────────────────────────

    #[test]
    fn toggle_root_selected() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![sd("s1", None, "/A"), sd("s2", None, "/B")],
            &mut badges,
        );

        assert!(!state.has_filter());
        assert!(state.root_filter().is_none());

        state.cursor = 0;
        assert!(state.toggle_selected());
        assert!(state.has_filter());
        assert!(state.is_root_selected("/A"));
        assert!(!state.is_root_selected("/B"));

        let filter = state.root_filter().expect("filter should be Some");
        assert!(filter.contains("/A"));
        assert!(!filter.contains("/B"));

        assert!(state.toggle_selected());
        assert!(!state.has_filter());
    }

    #[test]
    fn toggle_server_selected_in_workspace() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/A")], &mut badges);
        state.refresh_servers(
            &[
                make_server_row("rust-analyzer", "/A", "ready"),
                make_server_row("lua-ls", "/A", "ready"),
            ],
            &[],
        );

        state.cursor = 0;
        state.toggle_expanded();

        let srv_row = state
            .workspace_rows
            .iter()
            .position(|r| matches!(r, WorkspaceRow::Server(_, _)))
            .expect("should have server row");
        state.cursor = srv_row;

        assert!(state.toggle_selected());
        assert!(state.has_server_filter());
        assert!(state.is_server_selected("rust-analyzer", "/A"));
        assert!(!state.is_server_selected("lua-ls", "/A"));
    }

    // ── Expansion ───────────────────────────────────────────────

    #[test]
    fn toggle_expanded_only_on_root_rows() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", Some("claude"), "/A")], &mut badges);
        state.refresh_servers(&[make_server_row("ra", "/A", "ready")], &[]);

        state.cursor = 0;
        state.toggle_expanded();
        assert!(state.is_root_expanded("/A"));

        state.cursor = 1;
        let prev_len = state.workspace_rows.len();
        state.toggle_expanded();
        assert_eq!(
            state.workspace_rows.len(),
            prev_len,
            "toggle on non-root should be no-op"
        );
    }

    // ── Server popup ────────────────────────────────────────────

    #[test]
    fn cursor_server_index_on_server_row() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/A")], &mut badges);
        state.refresh_servers(&[make_server_row("ra", "/A", "ready")], &[]);

        state.cursor = 0;
        state.toggle_expanded();

        let srv_pos = state
            .workspace_rows
            .iter()
            .position(|r| matches!(r, WorkspaceRow::Server(_, _)))
            .expect("should have server row");
        state.cursor = srv_pos;
        assert_eq!(state.cursor_server_index(), Some(0));
    }

    #[test]
    fn cursor_server_index_on_non_server_row() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/A")], &mut badges);
        state.cursor = 0;
        assert!(state.cursor_server_index().is_none());
    }

    // ── Noise extraction ────────────────────────────────────────

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
    fn servers_need_refresh_detects_changes() {
        let mut state = SidebarState::new();
        let rows = vec![make_server_row("rust-analyzer", "/A", "ready")];
        state.refresh_servers(&rows, &[]);

        assert!(!state.servers_need_refresh(&rows, &[]));

        let changed = vec![make_server_row("rust-analyzer", "/A", "busy")];
        assert!(state.servers_need_refresh(&changed, &[]));
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

        assert!(!state.servers_need_refresh(&rows, &noise));

        let noise2 = vec![make_noise(
            "rust-analyzer",
            "/A",
            Some("Indexing"),
            Some(50),
            None,
        )];
        assert!(state.servers_need_refresh(&rows, &noise2));
    }

    // ── Rendering ───────────────────────────────────────────────

    #[test]
    fn render_workspaces_shows_roots() {
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
        let backend = TestBackend::new(30, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_workspaces(&state, area, frame.buffer_mut(), &theme, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("Catenary/"), "should show root: {content}");
        assert!(content.contains("OmniDSP/"), "should show root: {content}");
    }

    #[test]
    fn render_workspaces_expanded_shows_sections() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", Some("claude-code"), "/A")], &mut badges);
        state.refresh_servers(&[make_server_row("rust-analyzer", "/A", "ready")], &[]);

        state.cursor = 0;
        state.toggle_expanded();

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_workspaces(&state, area, frame.buffer_mut(), &theme, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("Connections:"),
            "should show connections header: {content}"
        );
        assert!(
            content.contains("claude-code"),
            "should show host: {content}"
        );
        assert!(
            content.contains("Servers:"),
            "should show servers header: {content}"
        );
        assert!(
            content.contains("rust-analyzer"),
            "should show server name: {content}"
        );
        assert!(
            content.contains("ready"),
            "should show server state: {content}"
        );
    }

    #[test]
    fn render_workspaces_server_without_root_suffix() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/home/user/Catenary")], &mut badges);
        state.refresh_servers(
            &[make_server_row(
                "rust-analyzer",
                "/home/user/Catenary",
                "ready",
            )],
            &[],
        );

        state.cursor = 0;
        state.toggle_expanded();

        let theme = crate::tui::theme::Theme::new();
        let backend = TestBackend::new(50, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_workspaces(&state, area, frame.buffer_mut(), &theme, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            !content.contains("(Catenary/)"),
            "server should not have root suffix: {content}"
        );
        assert!(
            content.contains("rust-analyzer"),
            "should show server name: {content}"
        );
    }

    #[test]
    fn render_workspaces_zero_area() {
        let state = SidebarState::new();
        let theme = crate::tui::theme::Theme::new();
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(area);
        let hits = render_workspaces(&state, area, &mut buf, &theme, true);
        assert!(hits.is_empty());
    }

    // ── Horizontal scroll ───────────────────────────────────────

    #[test]
    fn hscroll_left_clamps_at_zero() {
        let mut state = SidebarState::new();
        state.hscroll = 2;
        state.hscroll_left();
        assert_eq!(state.hscroll, 0);
        state.hscroll_left();
        assert_eq!(state.hscroll, 0);
    }

    #[test]
    fn hscroll_right_increments() {
        let mut state = SidebarState::new();
        state.hscroll_right();
        assert_eq!(state.hscroll, HSCROLL_STEP);
        state.hscroll_right();
        assert_eq!(state.hscroll, HSCROLL_STEP * 2);
    }

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

    // ── Visual selection ────────────────────────────────────────

    #[test]
    fn visual_selection_range() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(
            vec![sd("s1", None, "/A"), sd("s2", None, "/B")],
            &mut badges,
        );

        state.cursor = 0;
        state.start_visual();
        state.cursor = 1;
        assert_eq!(state.visual_range(), Some((0, 1)));

        state.exit_visual();
        assert!(state.visual_range().is_none());
    }

    #[test]
    fn yank_text_for_root() {
        let mut state = SidebarState::new();
        let mut badges = HexBadgeMap::new();
        state.refresh(vec![sd("s1", None, "/A")], &mut badges);

        state.cursor = 0;
        let text = state.yank_text().expect("should produce text");
        assert_eq!(text, "A/");
    }
}
