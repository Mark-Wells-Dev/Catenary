// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application state for the TUI.
//!
//! Owns the data source, stream state, and display configuration.
//! The event loop in [`super::run_loop`] drives state transitions.

use super::data::{DataSource, MessageTail};
use super::icons::IconSet;
use super::sidebar::SidebarState;
use super::stream::{PAGE_SIZE, PageRequest, StreamState};
use super::theme::Theme;

/// Which section has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusRegion {
    /// Session list panel.
    Sessions,
    /// Server dashboard panel.
    Servers,
    /// Keybinds panel (only focusable when expanded).
    Keybinds,
    /// Message stream.
    Stream,
}

/// Application state driving the TUI.
pub struct App<'a> {
    /// Semantic color theme.
    pub theme: &'a Theme,
    /// Resolved icon theme.
    pub icons: &'a IconSet,
    /// Data source for session and event data.
    pub data: Box<dyn DataSource>,
    /// Whether the user wants to quit.
    pub quit: bool,
    /// Which panel has keyboard focus.
    pub focus: FocusRegion,
    /// Whether the keybinds panel is expanded (`?` toggles).
    pub keybinds_expanded: bool,
    /// Session list sidebar state.
    pub sidebar: SidebarState,
    /// Unified message stream state.
    pub stream: StreamState,
    /// Tail reader for incremental message updates.
    pub tail: Option<Box<dyn MessageTail>>,
    /// Whether search input mode is active (`/` was pressed).
    pub search_active: bool,
    /// Text being typed into the search bar.
    pub search_input: String,
}

impl<'a> App<'a> {
    /// Create a new App, loading only the most recent page of scopes.
    ///
    /// # Errors
    ///
    /// Returns an error if loading messages fails.
    pub fn new(
        theme: &'a Theme,
        icons: &'a IconSet,
        data: Box<dyn DataSource>,
    ) -> anyhow::Result<Self> {
        let messages = data.recent_scopes(PAGE_SIZE)?;
        let tail = data.create_all_message_tail().ok();
        let mut stream = StreamState::new(messages);
        // Fewer entries than the page size means we loaded everything.
        stream.reached_beginning = stream.entries.len() < PAGE_SIZE;

        let mut app = Self {
            theme,
            icons,
            data,
            quit: false,
            focus: FocusRegion::Stream,
            keybinds_expanded: false,
            sidebar: SidebarState::new(),
            stream,
            tail,
            search_active: false,
            search_input: String::new(),
        };

        // Load initial session and server lists.
        app.refresh_sessions();
        app.refresh_servers();

        Ok(app)
    }

    /// Drain new messages from the tail reader into the stream.
    pub fn drain_tail(&mut self) {
        let Some(tail) = self.tail.as_mut() else {
            return;
        };
        let mut new_messages = Vec::new();
        while let Ok(Some(msg)) = tail.try_next_message() {
            new_messages.push(msg);
        }
        if !new_messages.is_empty() {
            self.stream.append(new_messages);
        }
    }

    /// Fetch a page if the cursor is near a paging boundary.
    pub fn fetch_page_if_needed(&mut self) {
        let Some(request) = self.stream.check_paging() else {
            return;
        };

        match request {
            PageRequest::Older(before_id) => {
                if let Ok(messages) = self.data.older_scopes(before_id, None, PAGE_SIZE) {
                    self.stream.prepend_page(messages);
                }
            }
            PageRequest::FillGap {
                after_id,
                before_id,
                from_bottom,
            } => {
                let messages = if from_bottom {
                    // Load the newest scopes in the gap (closest to bottom).
                    self.data.older_scopes(before_id, Some(after_id), PAGE_SIZE)
                } else {
                    // Load the oldest scopes in the gap (closest to top).
                    self.data.newer_scopes(after_id, Some(before_id), PAGE_SIZE)
                };
                if let Ok(messages) = messages {
                    self.stream.fill_gap(messages);
                }
            }
        }
    }

    /// Load the oldest page and create a gap (Home key).
    pub fn jump_to_beginning(&mut self) {
        // If already at the beginning with a gap, just move cursor.
        if self.stream.gap_offset.is_some() || self.stream.reached_beginning {
            self.stream.scroll_position = 0;
            self.stream.cursor = 0;
            self.stream.auto_scroll = false;
            return;
        }

        if let Ok(messages) = self.data.oldest_scopes(PAGE_SIZE) {
            self.stream.load_oldest_page(messages);
        }
    }

    /// Toggle selection on the sidebar's cursor session entry and update the
    /// stream's session filter.
    pub fn toggle_session_selection(&mut self) {
        if self.sidebar.toggle_selected() {
            self.stream
                .set_session_filter(self.sidebar.session_filter());
        }
    }

    /// Toggle selection on the sidebar's cursor server entry and update the
    /// stream's server filter.
    pub fn toggle_server_selection(&mut self) {
        if self.sidebar.toggle_server_selected() {
            self.stream.set_server_filter(self.sidebar.server_filter());
        }
    }

    /// Cycle focus: Sessions → Servers → [Keybinds] → Stream → Sessions.
    ///
    /// Keybinds is skipped when collapsed.
    pub const fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            FocusRegion::Sessions => FocusRegion::Servers,
            FocusRegion::Servers => {
                if self.keybinds_expanded {
                    FocusRegion::Keybinds
                } else {
                    FocusRegion::Stream
                }
            }
            FocusRegion::Keybinds => FocusRegion::Stream,
            FocusRegion::Stream => FocusRegion::Sessions,
        };
    }

    /// Cycle focus in reverse: Sessions → Stream → [Keybinds] → Servers → Sessions.
    ///
    /// Keybinds is skipped when collapsed.
    pub const fn cycle_focus_back(&mut self) {
        self.focus = match self.focus {
            FocusRegion::Sessions => FocusRegion::Stream,
            FocusRegion::Servers => FocusRegion::Sessions,
            FocusRegion::Keybinds => FocusRegion::Servers,
            FocusRegion::Stream => {
                if self.keybinds_expanded {
                    FocusRegion::Keybinds
                } else {
                    FocusRegion::Servers
                }
            }
        };
    }

    /// Toggle keybinds panel expansion. Moves focus away when collapsing.
    pub fn toggle_keybinds(&mut self) {
        self.keybinds_expanded = !self.keybinds_expanded;
        if !self.keybinds_expanded && self.focus == FocusRegion::Keybinds {
            self.focus = FocusRegion::Servers;
        }
    }

    /// Refresh the sidebar session list if the alive set has changed.
    ///
    /// Uses a two-phase query: lightweight ID check first, full metadata
    /// only when the set changes. Silently ignores query failures.
    pub fn refresh_sessions(&mut self) {
        let Ok(current_ids) = self.data.list_alive_session_ids() else {
            return;
        };
        if !self.sidebar.needs_refresh(&current_ids) {
            return;
        }
        let Ok(rows) = self.data.list_sessions() else {
            return;
        };
        let sessions: Vec<_> = rows
            .into_iter()
            .filter(|r| r.alive)
            .map(|r| (r.info.id, r.info.client_name, r.info.workspace))
            .collect();
        let had_filter = self.sidebar.has_filter();
        self.sidebar.refresh(sessions, &mut self.stream.badges);
        // If a selected session disconnected, the selected set shrank.
        // Propagate to stream so filtered entries reappear if needed.
        if had_filter {
            self.stream
                .set_session_filter(self.sidebar.session_filter());
        }
    }

    /// Refresh the sidebar server list from the database.
    ///
    /// Queries `language_servers` and server noise (progress, log/show
    /// messages) and updates the server section. If a selected server
    /// disappears, the server filter is propagated to the stream.
    /// Silently ignores query failures.
    pub fn refresh_servers(&mut self) {
        let Ok(rows) = self.data.list_server_statuses() else {
            return;
        };
        let noise = self.data.list_server_noise().unwrap_or_default();
        if !self.sidebar.servers_need_refresh(&rows, &noise) {
            return;
        }
        let had_filter = self.sidebar.has_server_filter();
        self.sidebar.refresh_servers(&rows, &noise);
        // If a selected server disappeared, the selected set shrank.
        // Propagate to stream so filtered entries reappear if needed.
        if had_filter {
            self.stream.set_server_filter(self.sidebar.server_filter());
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::IconConfig;
    use crate::tui::data::MockDataSource;

    fn make_app<'a>(theme: &'a Theme, icons: &'a IconSet) -> App<'a> {
        let data: Box<dyn DataSource> = Box::new(MockDataSource {
            sessions: Vec::new(),
            messages: HashMap::new(),
            tail_messages: HashMap::new(),
            server_statuses: Vec::new(),
            server_noise: Vec::new(),
        });
        App::new(theme, icons, data).expect("mock app creation")
    }

    #[test]
    fn cycle_focus_skips_keybinds_when_collapsed() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.focus = FocusRegion::Sessions;
        assert!(!app.keybinds_expanded);

        app.cycle_focus();
        assert_eq!(app.focus, FocusRegion::Servers);
        app.cycle_focus();
        assert_eq!(app.focus, FocusRegion::Stream);
        app.cycle_focus();
        assert_eq!(app.focus, FocusRegion::Sessions);
    }

    #[test]
    fn cycle_focus_includes_keybinds_when_expanded() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.keybinds_expanded = true;
        app.focus = FocusRegion::Servers;

        app.cycle_focus();
        assert_eq!(app.focus, FocusRegion::Keybinds);
        app.cycle_focus();
        assert_eq!(app.focus, FocusRegion::Stream);
    }

    #[test]
    fn cycle_focus_back_skips_keybinds_when_collapsed() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.focus = FocusRegion::Stream;
        assert!(!app.keybinds_expanded);

        app.cycle_focus_back();
        assert_eq!(app.focus, FocusRegion::Servers);
        app.cycle_focus_back();
        assert_eq!(app.focus, FocusRegion::Sessions);
        app.cycle_focus_back();
        assert_eq!(app.focus, FocusRegion::Stream);
    }

    #[test]
    fn cycle_focus_back_includes_keybinds_when_expanded() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.keybinds_expanded = true;
        app.focus = FocusRegion::Stream;

        app.cycle_focus_back();
        assert_eq!(app.focus, FocusRegion::Keybinds);
        app.cycle_focus_back();
        assert_eq!(app.focus, FocusRegion::Servers);
    }

    #[test]
    fn toggle_keybinds_flips_expanded() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        assert!(!app.keybinds_expanded);
        app.toggle_keybinds();
        assert!(app.keybinds_expanded);
        app.toggle_keybinds();
        assert!(!app.keybinds_expanded);
    }

    #[test]
    fn toggle_keybinds_moves_focus_on_collapse() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.keybinds_expanded = true;
        app.focus = FocusRegion::Keybinds;
        app.toggle_keybinds();
        assert_eq!(app.focus, FocusRegion::Servers);
    }
}
