// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application state for the `state.json` dashboard.
//!
//! The TUI holds the latest snapshot and renders three boards from it — server
//! health, sessions, and the alerts ring. It is a pure file reader: a file-watch
//! on the snapshot drives [`App::reload`]; there is no database, no firehose, no
//! socket client (observability ticket 06).

use super::data::DataSource;
use super::icons::IconSet;
use super::theme::Theme;
use crate::state_snapshot::Snapshot;

/// Which board currently has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// Server health board (top-left).
    Servers,
    /// Session board (bottom-left).
    Sessions,
    /// Activity ring (top-right pane) — curated milestones.
    Activity,
    /// Alerts ring (bottom-right pane).
    Alerts,
}

impl Focus {
    /// Next board in the cycle
    /// (Servers → Sessions → Activity → Alerts → Servers).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Servers => Self::Sessions,
            Self::Sessions => Self::Activity,
            Self::Activity => Self::Alerts,
            Self::Alerts => Self::Servers,
        }
    }

    /// Previous board in the cycle.
    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::Servers => Self::Alerts,
            Self::Sessions => Self::Servers,
            Self::Activity => Self::Sessions,
            Self::Alerts => Self::Activity,
        }
    }
}

/// Cursor + scroll for a single board, indexed by **entry** (not by rendered
/// line — a server/session entry spans two lines).
#[derive(Debug, Default, Clone, Copy)]
pub struct Board {
    /// Selected entry index.
    pub cursor: usize,
    /// First visible entry index.
    pub scroll: usize,
    /// Entries visible in the panel, refreshed each render so key handlers can
    /// page and keep the cursor on screen without re-deriving panel height.
    pub visible: usize,
}

impl Board {
    /// Clamp the cursor and scroll to `len` entries (after a reload may shrink
    /// the list). An empty list resets both to zero.
    fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = self.cursor.min(len - 1);
        self.scroll = self.scroll.min(self.cursor);
    }

    /// Move the cursor up `n` entries, scrolling to keep it visible.
    const fn up(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
    }

    /// Move the cursor down `n` entries within `len`, scrolling to keep it
    /// visible.
    fn down(&mut self, n: usize, len: usize) {
        if len == 0 {
            return;
        }
        self.cursor = (self.cursor + n).min(len - 1);
        if self.visible > 0 && self.cursor >= self.scroll + self.visible {
            self.scroll = self.cursor + 1 - self.visible;
        }
    }

    /// Re-clamp scroll against the current visible window (called at render with
    /// the freshly measured `visible`).
    pub fn settle(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = self.cursor.min(len - 1);
        if self.visible > 0 && self.cursor >= self.scroll + self.visible {
            self.scroll = self.cursor + 1 - self.visible;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
    }
}

/// Dashboard application state.
pub struct App<'a> {
    /// Semantic color theme.
    pub theme: &'a Theme,
    /// Resolved icon theme.
    pub icons: &'a IconSet,
    /// Snapshot data source (`state.json` in production).
    pub data: Box<dyn DataSource>,
    /// The latest snapshot, re-loaded on file-watch / tick.
    pub snapshot: Snapshot,
    /// Whether the user wants to quit.
    pub quit: bool,
    /// Which board has focus.
    pub focus: Focus,
    /// Server board cursor/scroll.
    pub servers: Board,
    /// Session board cursor/scroll.
    pub sessions: Board,
    /// Activity ring cursor/scroll.
    pub activity: Board,
    /// Alerts ring cursor/scroll.
    pub alerts: Board,
    /// Whether the keybinds panel is expanded (`?` toggles).
    pub keybinds_expanded: bool,
    /// Left-column width as a percentage of the terminal (clamped 10..=90).
    pub sidebar_pct: u16,
    /// Whether the user is dragging the panel divider.
    pub dragging_divider: bool,
}

impl<'a> App<'a> {
    /// Build a new dashboard, loading the initial snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial snapshot read fails (a parse error on an
    /// existing file; a missing file is not an error).
    pub fn new(
        theme: &'a Theme,
        icons: &'a IconSet,
        data: Box<dyn DataSource>,
    ) -> anyhow::Result<Self> {
        let snapshot = data.load()?;
        Ok(Self {
            theme,
            icons,
            data,
            snapshot,
            quit: false,
            focus: Focus::Servers,
            servers: Board::default(),
            sessions: Board::default(),
            activity: Board::default(),
            alerts: Board::default(),
            keybinds_expanded: false,
            sidebar_pct: 50,
            dragging_divider: false,
        })
    }

    /// Re-load the snapshot. On read/parse failure the previous snapshot is
    /// kept (a transient torn read never blanks the dashboard).
    pub fn reload(&mut self) {
        if let Ok(snapshot) = self.data.load() {
            self.snapshot = snapshot;
            self.clamp_cursors();
        }
    }

    /// Re-clamp every board against its current entry count.
    fn clamp_cursors(&mut self) {
        self.servers.clamp(self.snapshot.servers.len());
        self.sessions.clamp(self.snapshot.sessions.len());
        self.activity.clamp(self.snapshot.activity.len());
        self.alerts.clamp(self.snapshot.alerts.len());
    }

    /// Whether a daemon snapshot is present (the file existed and parsed with a
    /// generation timestamp). When false, the dashboard shows a waiting state.
    #[must_use]
    pub const fn daemon_present(&self) -> bool {
        !self.snapshot.daemon.generated_at.is_empty()
    }

    /// Entry count for the focused board.
    #[must_use]
    pub const fn focused_len(&self) -> usize {
        match self.focus {
            Focus::Servers => self.snapshot.servers.len(),
            Focus::Sessions => self.snapshot.sessions.len(),
            Focus::Activity => self.snapshot.activity.len(),
            Focus::Alerts => self.snapshot.alerts.len(),
        }
    }

    /// Mutable handle to the focused board's cursor/scroll.
    pub const fn focused_board(&mut self) -> &mut Board {
        match self.focus {
            Focus::Servers => &mut self.servers,
            Focus::Sessions => &mut self.sessions,
            Focus::Activity => &mut self.activity,
            Focus::Alerts => &mut self.alerts,
        }
    }

    /// Advance focus to the next board.
    pub const fn cycle_focus(&mut self) {
        self.focus = self.focus.next();
    }

    /// Advance focus to the previous board.
    pub const fn cycle_focus_back(&mut self) {
        self.focus = self.focus.prev();
    }

    /// Move the focused cursor up `n` entries.
    pub const fn cursor_up(&mut self, n: usize) {
        self.focused_board().up(n);
    }

    /// Move the focused cursor down `n` entries.
    pub fn cursor_down(&mut self, n: usize) {
        let len = self.focused_len();
        self.focused_board().down(n, len);
    }

    /// Page up by the focused board's visible window.
    pub fn page_up(&mut self) {
        let page = self.focused_board().visible.max(1);
        self.cursor_up(page);
    }

    /// Page down by the focused board's visible window.
    pub fn page_down(&mut self) {
        let page = self.focused_board().visible.max(1);
        self.cursor_down(page);
    }

    /// Jump the focused cursor to the first entry.
    pub const fn jump_home(&mut self) {
        let board = self.focused_board();
        board.cursor = 0;
        board.scroll = 0;
    }

    /// Jump the focused cursor to the last entry.
    pub const fn jump_end(&mut self) {
        let len = self.focused_len();
        if len > 0 {
            self.focused_board().cursor = len - 1;
        }
    }

    /// The scope id (or alert text) to yank for the current selection — the
    /// bridge into `catenary query`. `None` when the focused board is empty.
    #[must_use]
    pub fn selected_yank_text(&self) -> Option<String> {
        match self.focus {
            Focus::Servers => self
                .snapshot
                .servers
                .get(self.servers.cursor)
                .map(|s| s.id.clone()),
            Focus::Sessions => self
                .snapshot
                .sessions
                .get(self.sessions.cursor)
                .map(|s| s.id.clone()),
            Focus::Activity => self.snapshot.activity.get(self.activity.cursor).map(|m| {
                // Prefer the scope (query bridge); fall back to the summary.
                m.scope
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map_or_else(|| m.summary.clone(), ToString::to_string)
            }),
            Focus::Alerts => self.snapshot.alerts.get(self.alerts.cursor).map(|a| {
                // Prefer the scope (query bridge); fall back to the message.
                a.scope
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map_or_else(|| a.text.clone(), ToString::to_string)
            }),
        }
    }

    /// Toggle the keybinds help panel.
    pub const fn toggle_keybinds(&mut self) {
        self.keybinds_expanded = !self.keybinds_expanded;
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::state_snapshot::{Alert, ServerEntry, SessionEntry};
    use crate::tui::data::MockDataSource;

    fn snapshot_with(servers: usize, sessions: usize, alerts: usize) -> Snapshot {
        Snapshot {
            schema: 1,
            servers: (0..servers)
                .map(|i| ServerEntry {
                    id: format!("ra-{i}@/p"),
                    server: format!("ra-{i}"),
                    state: "healthy".to_string(),
                    ..ServerEntry::default()
                })
                .collect(),
            sessions: (0..sessions)
                .map(|i| SessionEntry {
                    id: format!("mcp:{i}"),
                    ..SessionEntry::default()
                })
                .collect(),
            alerts: (0..alerts)
                .map(|i| Alert {
                    at: "2026-06-08T14:32:00Z".to_string(),
                    level: "warn".to_string(),
                    text: format!("alert {i}"),
                    ..Alert::default()
                })
                .collect(),
            ..Snapshot::default()
        }
    }

    fn app_with<'a>(theme: &'a Theme, icons: &'a IconSet, snap: Snapshot) -> App<'a> {
        App::new(theme, icons, Box::new(MockDataSource::new(snap))).expect("app")
    }

    #[test]
    fn focus_cycles_through_four_boards() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snapshot_with(1, 1, 1));
        assert_eq!(app.focus, Focus::Servers);
        app.cycle_focus();
        assert_eq!(app.focus, Focus::Sessions);
        app.cycle_focus();
        assert_eq!(app.focus, Focus::Activity);
        app.cycle_focus();
        assert_eq!(app.focus, Focus::Alerts);
        app.cycle_focus();
        assert_eq!(app.focus, Focus::Servers);
        app.cycle_focus_back();
        assert_eq!(app.focus, Focus::Alerts);
    }

    #[test]
    fn cursor_clamps_to_focused_board_len() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snapshot_with(3, 0, 0));
        app.focused_board().visible = 10;
        app.cursor_down(100);
        assert_eq!(app.servers.cursor, 2, "clamped to last server");
        app.jump_home();
        assert_eq!(app.servers.cursor, 0);
        app.jump_end();
        assert_eq!(app.servers.cursor, 2);
    }

    #[test]
    fn empty_board_navigation_is_safe() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snapshot_with(0, 0, 0));
        app.cursor_down(1);
        app.cursor_up(1);
        app.jump_end();
        assert_eq!(app.servers.cursor, 0);
        assert!(app.selected_yank_text().is_none());
    }

    #[test]
    fn yank_returns_scope_id_per_board() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snapshot_with(2, 2, 0));
        assert_eq!(app.selected_yank_text().as_deref(), Some("ra-0@/p"));
        app.cursor_down(1);
        assert_eq!(app.selected_yank_text().as_deref(), Some("ra-1@/p"));
        app.focus = Focus::Sessions;
        assert_eq!(app.selected_yank_text().as_deref(), Some("mcp:0"));
    }

    #[test]
    fn alert_yank_prefers_scope_then_text() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut snap = snapshot_with(0, 0, 1);
        snap.alerts[0].scope = Some("rust-analyzer@/p".to_string());
        let mut app = app_with(&theme, &icons, snap);
        app.focus = Focus::Alerts;
        assert_eq!(
            app.selected_yank_text().as_deref(),
            Some("rust-analyzer@/p")
        );
    }

    #[test]
    fn reload_clamps_cursor_when_list_shrinks() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        // Start with 5 servers, cursor at the end, then reload a 2-server
        // snapshot from the same source by swapping it in.
        let mut app = app_with(&theme, &icons, snapshot_with(5, 0, 0));
        app.focused_board().visible = 10;
        app.jump_end();
        assert_eq!(app.servers.cursor, 4);
        app.data = Box::new(MockDataSource::new(snapshot_with(2, 0, 0)));
        app.reload();
        assert_eq!(app.servers.cursor, 1, "cursor clamped to new last entry");
    }

    #[test]
    fn daemon_present_reflects_generated_at() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let app = app_with(&theme, &icons, Snapshot::default());
        assert!(!app.daemon_present(), "empty snapshot = waiting");

        let mut snap = snapshot_with(1, 0, 0);
        snap.daemon.generated_at = "2026-06-08T14:32:10Z".to_string();
        let app = app_with(&theme, &icons, snap);
        assert!(app.daemon_present());
    }
}
