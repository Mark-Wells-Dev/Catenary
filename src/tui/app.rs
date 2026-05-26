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

/// Display level threshold for message queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelThreshold {
    /// Show info, warn, error. Default.
    Info,
    /// Show everything including debug.
    Debug,
}

impl LevelThreshold {
    /// Whether to include debug-level messages in queries.
    #[must_use]
    pub const fn include_debug(self) -> bool {
        matches!(self, Self::Debug)
    }

    /// Toggle between Info and Debug.
    pub const fn toggle(&mut self) {
        *self = match self {
            Self::Info => Self::Debug,
            Self::Debug => Self::Info,
        };
    }
}

/// Which panel has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusRegion {
    /// Sidebar session list.
    Sidebar,
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
    /// Current display level threshold.
    pub level_threshold: LevelThreshold,
    /// Which panel has keyboard focus.
    pub focus: FocusRegion,
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
        let include_debug = false;
        let messages = data.recent_scopes(PAGE_SIZE, include_debug)?;
        let tail = data.create_all_message_tail(include_debug).ok();
        let mut stream = StreamState::new(messages);
        // Fewer entries than the page size means we loaded everything.
        stream.reached_beginning = stream.entries.len() < PAGE_SIZE;

        let mut app = Self {
            theme,
            icons,
            data,
            quit: false,
            level_threshold: LevelThreshold::Info,
            focus: FocusRegion::Stream,
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

    /// Reload the most recent page (e.g., after toggling severity threshold).
    ///
    /// # Errors
    ///
    /// Returns an error if loading messages fails.
    pub fn reload_messages(&mut self) -> anyhow::Result<()> {
        let include_debug = self.level_threshold.include_debug();
        let messages = self.data.recent_scopes(PAGE_SIZE, include_debug)?;
        self.tail = self.data.create_all_message_tail(include_debug).ok();
        self.stream = StreamState::new(messages);
        self.stream.reached_beginning = self.stream.entries.len() < PAGE_SIZE;
        Ok(())
    }

    /// Fetch a page if the cursor is near a paging boundary.
    pub fn fetch_page_if_needed(&mut self) {
        let Some(request) = self.stream.check_paging() else {
            return;
        };
        let include_debug = self.level_threshold.include_debug();

        match request {
            PageRequest::Older(before_id) => {
                if let Ok(messages) =
                    self.data
                        .older_scopes(before_id, None, PAGE_SIZE, include_debug)
                {
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
                    self.data
                        .older_scopes(before_id, Some(after_id), PAGE_SIZE, include_debug)
                } else {
                    // Load the oldest scopes in the gap (closest to top).
                    self.data
                        .newer_scopes(after_id, Some(before_id), PAGE_SIZE, include_debug)
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

        let include_debug = self.level_threshold.include_debug();
        if let Ok(messages) = self.data.oldest_scopes(PAGE_SIZE, include_debug) {
            self.stream.load_oldest_page(messages);
        }
    }

    /// Toggle selection on the sidebar's cursor entry and update the
    /// stream's session filter.
    pub fn toggle_session_selection(&mut self) {
        if self.sidebar.toggle_selected() {
            self.stream
                .set_session_filter(self.sidebar.session_filter());
        }
    }

    /// Toggle focus between sidebar and stream.
    pub const fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            FocusRegion::Sidebar => FocusRegion::Stream,
            FocusRegion::Stream => FocusRegion::Sidebar,
        };
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
    /// Queries `language_servers` and updates the server section.
    /// Silently ignores query failures.
    pub fn refresh_servers(&mut self) {
        let Ok(rows) = self.data.list_server_statuses() else {
            return;
        };
        if !self.sidebar.servers_need_refresh(&rows) {
            return;
        }
        self.sidebar.refresh_servers(&rows);
    }
}
