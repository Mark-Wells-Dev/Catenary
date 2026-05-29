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
    /// Sidebar session list.
    Sessions,
    /// Sidebar server list.
    Servers,
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
    /// User preference: sidebar visible (toggled by `b` keybinding).
    pub sidebar_visible: bool,
    /// Session list sidebar state.
    pub sidebar: SidebarState,
    /// Unified message stream state.
    pub stream: StreamState,
    /// Tail reader for incremental message updates.
    pub tail: Option<Box<dyn MessageTail>>,
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
            sidebar_visible: true,
            sidebar: SidebarState::new(),
            stream,
            tail,
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

    /// Cycle focus: Sessions → Servers → Stream → Sessions.
    pub const fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            FocusRegion::Sessions => FocusRegion::Servers,
            FocusRegion::Servers => FocusRegion::Stream,
            FocusRegion::Stream => FocusRegion::Sessions,
        };
    }

    /// Cycle focus in reverse: Sessions → Stream → Servers → Sessions.
    pub const fn cycle_focus_back(&mut self) {
        self.focus = match self.focus {
            FocusRegion::Sessions => FocusRegion::Stream,
            FocusRegion::Servers => FocusRegion::Sessions,
            FocusRegion::Stream => FocusRegion::Servers,
        };
    }

    /// Toggle sidebar visibility. Moves focus to the stream when hiding.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        if !self.sidebar_visible && self.focus != FocusRegion::Stream {
            self.focus = FocusRegion::Stream;
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
    fn toggle_sidebar_flips_visibility() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        assert!(app.sidebar_visible);
        app.toggle_sidebar();
        assert!(!app.sidebar_visible);
        app.toggle_sidebar();
        assert!(app.sidebar_visible);
    }

    #[test]
    fn toggle_sidebar_moves_focus_from_sessions_to_stream() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.focus = FocusRegion::Sessions;
        app.toggle_sidebar();
        assert_eq!(app.focus, FocusRegion::Stream);
    }

    #[test]
    fn toggle_sidebar_moves_focus_from_servers_to_stream() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.focus = FocusRegion::Servers;
        app.toggle_sidebar();
        assert_eq!(app.focus, FocusRegion::Stream);
    }

    #[test]
    fn toggle_sidebar_keeps_focus_on_stream() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.focus = FocusRegion::Stream;
        app.toggle_sidebar();
        assert_eq!(app.focus, FocusRegion::Stream);
    }

    #[test]
    fn toggle_sidebar_preserves_filter_state() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        // Inject a session and select it.
        app.sidebar.refresh(
            vec![("s1".into(), Some("claude".into()), "/project".into())],
            &mut app.stream.badges,
        );
        app.sidebar.cursor = 0;
        app.sidebar.toggle_selected();
        assert!(app.sidebar.has_filter());

        // Hide and show sidebar — filter survives.
        app.toggle_sidebar();
        assert!(app.sidebar.has_filter());
        app.toggle_sidebar();
        assert!(app.sidebar.has_filter());
    }

    #[test]
    fn show_sidebar_restores_after_toggle() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);

        app.toggle_sidebar();
        assert!(!app.sidebar_visible);
        app.toggle_sidebar();
        assert!(app.sidebar_visible);
        // Focus should be on stream (moved there on hide, stays on show).
        assert_eq!(app.focus, FocusRegion::Stream);
    }
}
