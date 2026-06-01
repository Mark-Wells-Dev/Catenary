// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application state for the TUI.
//!
//! Owns the data source, stream state, and display configuration.
//! The event loop in [`super::run_loop`] drives state transitions.

use super::data::{DataSource, MessageTail};
use super::icons::IconSet;
use super::popup::ServerPopup;
use super::sidebar::{SessionData, SidebarState};
use super::stream::{PAGE_SIZE, PageRequest, StreamState};
use super::theme::Theme;

/// Which section has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusRegion {
    /// Unified workspace panel (connections + servers).
    Workspaces,
    /// Keybinds panel (only focusable when expanded).
    Keybinds,
    /// Message stream.
    Stream,
}

/// Left-column layout preference set by the user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LeftLayout {
    /// Two panels stacked vertically (Workspaces, Keybinds).
    #[default]
    Quadrant,
    /// Tab-stacked: one panel visible at a time, cycled with `b`.
    Stacked,
}

/// Effective layout mode, computed from [`LeftLayout`] and terminal size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EffectiveLayout {
    /// Quadrant: left column has two panels, Traffic on right.
    #[default]
    Quadrant,
    /// Stacked sidebar: left column is a tab stack, Traffic on right.
    Stacked,
    /// Full-width tab stack: all panels (including Traffic) in one stack.
    FullStack,
}

/// Terminal height below which the left column degrades to tab stacking.
const SHORT_THRESHOLD: u16 = 12;

/// Terminal width below which everything degrades to a single full-width
/// tab stack.
const NARROW_THRESHOLD: u16 = 50;

/// Application state driving the TUI.
#[allow(
    clippy::struct_excessive_bools,
    reason = "TUI state naturally has independent boolean flags"
)]
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
    /// User's left-column layout preference.
    pub left_layout: LeftLayout,
    /// Active tab in the left-column stack (0=Workspaces, 1=Keybinds).
    pub active_left_tab: usize,
    /// Effective layout mode, recomputed each frame from [`left_layout`] and
    /// terminal dimensions.
    pub effective: EffectiveLayout,
    /// Sidebar width as a percentage of the terminal (clamped to 10..=90).
    pub sidebar_pct: u16,
    /// Whether the user is dragging the panel divider.
    pub dragging_divider: bool,
    /// Unified workspace sidebar state.
    pub sidebar: SidebarState,
    /// Unified message stream state.
    pub stream: StreamState,
    /// Tail reader for incremental message updates.
    pub tail: Option<Box<dyn MessageTail>>,
    /// Whether search input mode is active (`/` was pressed).
    pub search_active: bool,
    /// Text being typed into the search bar.
    pub search_input: String,
    /// Server message popup overlay (shown when user presses Enter on a server).
    pub popup: Option<ServerPopup>,
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
        stream.reached_beginning = stream.entries.len() < PAGE_SIZE;

        let mut app = Self {
            theme,
            icons,
            data,
            quit: false,
            focus: FocusRegion::Stream,
            keybinds_expanded: false,
            left_layout: LeftLayout::Quadrant,
            active_left_tab: 0,
            effective: EffectiveLayout::Quadrant,
            sidebar_pct: 50,
            dragging_divider: false,
            sidebar: SidebarState::new(),
            stream,
            tail,
            search_active: false,
            search_input: String::new(),
            popup: None,
        };

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
        let Some(PageRequest::Older(before_id)) = self.stream.check_paging() else {
            return;
        };

        if let Ok(messages) = self.data.older_scopes(before_id, PAGE_SIZE) {
            self.stream.prepend_page(messages);
        }
    }

    /// Jump to the top of loaded content (Home key).
    pub const fn jump_to_beginning(&mut self) {
        self.stream.scroll_position = 0;
        self.stream.cursor = 0;
        self.stream.auto_scroll = false;
    }

    /// Toggle selection on the sidebar's current cursor item and update
    /// the stream filters.
    pub fn toggle_workspace_selection(&mut self) {
        if self.sidebar.toggle_selected() {
            self.stream.set_root_filter(self.sidebar.root_filter());
            self.stream.set_server_filter(self.sidebar.server_filter());
        }
    }

    /// Map a left-tab index to the corresponding [`FocusRegion`].
    #[must_use]
    pub const fn tab_focus(tab: usize) -> FocusRegion {
        match tab {
            0 => FocusRegion::Workspaces,
            _ => FocusRegion::Keybinds,
        }
    }

    /// Recompute `effective` from the user preference and terminal size.
    pub fn update_effective(&mut self, width: u16, height: u16) {
        let new = if width < NARROW_THRESHOLD {
            EffectiveLayout::FullStack
        } else if height < SHORT_THRESHOLD {
            EffectiveLayout::Stacked
        } else {
            match self.left_layout {
                LeftLayout::Quadrant => EffectiveLayout::Quadrant,
                LeftLayout::Stacked => EffectiveLayout::Stacked,
            }
        };

        // Entering stacked/fullstack: sync active_left_tab to current focus.
        if new != EffectiveLayout::Quadrant && self.effective == EffectiveLayout::Quadrant {
            match self.focus {
                FocusRegion::Workspaces => self.active_left_tab = 0,
                FocusRegion::Keybinds => self.active_left_tab = 1,
                FocusRegion::Stream => {}
            }
        }

        // Returning to quadrant: if focused on collapsed keybinds, fall back.
        if new == EffectiveLayout::Quadrant
            && self.effective != EffectiveLayout::Quadrant
            && self.focus == FocusRegion::Keybinds
            && !self.keybinds_expanded
        {
            self.focus = FocusRegion::Workspaces;
        }

        self.effective = new;
    }

    /// Cycle the left-column tab (`b` key).
    ///
    /// - **Quadrant**: enters stacked mode, showing the current left tab.
    /// - **Stacked**: cycles tabs (Workspaces → Keybinds), then
    ///   returns to quadrant if the terminal permits.
    /// - **`FullStack`**: cycles through all three tabs including Traffic.
    pub const fn cycle_left_tab(&mut self) {
        match self.effective {
            EffectiveLayout::FullStack => {
                if matches!(self.focus, FocusRegion::Stream) {
                    self.active_left_tab = 0;
                    self.focus = FocusRegion::Workspaces;
                } else if self.active_left_tab >= 1 {
                    self.focus = FocusRegion::Stream;
                } else {
                    self.active_left_tab += 1;
                    self.focus = Self::tab_focus(self.active_left_tab);
                }
            }
            EffectiveLayout::Stacked => {
                if self.active_left_tab >= 1 {
                    self.active_left_tab = 0;
                    self.left_layout = LeftLayout::Quadrant;
                } else {
                    self.active_left_tab += 1;
                }
                self.focus = Self::tab_focus(self.active_left_tab);
            }
            EffectiveLayout::Quadrant => {
                self.left_layout = LeftLayout::Stacked;
                self.focus = Self::tab_focus(self.active_left_tab);
            }
        }
    }

    /// Switch directly to a specific left tab, entering stacked mode if needed.
    pub fn set_left_tab(&mut self, tab: usize) {
        if tab > 1 {
            self.focus = FocusRegion::Stream;
            return;
        }
        self.active_left_tab = tab;
        self.focus = Self::tab_focus(tab);
        if self.effective == EffectiveLayout::Quadrant {
            self.left_layout = LeftLayout::Stacked;
        }
    }

    /// Cycle focus: in quadrant mode cycles all panels; in stacked/full-stack
    /// mode toggles between the left stack and Traffic.
    pub const fn cycle_focus(&mut self) {
        match self.effective {
            EffectiveLayout::Quadrant => {
                self.focus = match self.focus {
                    FocusRegion::Workspaces => {
                        if self.keybinds_expanded {
                            FocusRegion::Keybinds
                        } else {
                            FocusRegion::Stream
                        }
                    }
                    FocusRegion::Keybinds => FocusRegion::Stream,
                    FocusRegion::Stream => FocusRegion::Workspaces,
                };
            }
            EffectiveLayout::Stacked | EffectiveLayout::FullStack => {
                self.focus = if matches!(self.focus, FocusRegion::Stream) {
                    Self::tab_focus(self.active_left_tab)
                } else {
                    FocusRegion::Stream
                };
            }
        }
    }

    /// Cycle focus in reverse.
    pub const fn cycle_focus_back(&mut self) {
        match self.effective {
            EffectiveLayout::Quadrant => {
                self.focus = match self.focus {
                    FocusRegion::Workspaces => FocusRegion::Stream,
                    FocusRegion::Keybinds => FocusRegion::Workspaces,
                    FocusRegion::Stream => {
                        if self.keybinds_expanded {
                            FocusRegion::Keybinds
                        } else {
                            FocusRegion::Workspaces
                        }
                    }
                };
            }
            EffectiveLayout::Stacked | EffectiveLayout::FullStack => {
                self.focus = if matches!(self.focus, FocusRegion::Stream) {
                    Self::tab_focus(self.active_left_tab)
                } else {
                    FocusRegion::Stream
                };
            }
        }
    }

    /// Toggle keybinds: in quadrant mode toggles expansion; in stacked/full-stack
    /// mode jumps to the Keybinds tab.
    pub fn toggle_keybinds(&mut self) {
        match self.effective {
            EffectiveLayout::Quadrant => {
                self.keybinds_expanded = !self.keybinds_expanded;
                if !self.keybinds_expanded && self.focus == FocusRegion::Keybinds {
                    self.focus = FocusRegion::Workspaces;
                }
            }
            EffectiveLayout::Stacked | EffectiveLayout::FullStack => {
                self.active_left_tab = 1;
                self.focus = FocusRegion::Keybinds;
            }
        }
    }

    /// Refresh the sidebar session list if the alive set has changed.
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
            .map(|r| SessionData {
                id: r.info.id,
                client_name: r.info.client_name,
                workspace: r.info.workspace,
                languages: r.languages,
            })
            .collect();
        let had_filter = self.sidebar.has_filter();
        self.sidebar.refresh(sessions, &mut self.stream.badges);
        if had_filter {
            self.stream.set_root_filter(self.sidebar.root_filter());
        }
    }

    /// Refresh the sidebar server list from the database.
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
        if had_filter {
            self.stream.set_server_filter(self.sidebar.server_filter());
        }
    }

    /// Open the server message popup for the server at the current cursor.
    pub fn open_server_popup(&mut self) {
        let Some(srv_idx) = self.sidebar.cursor_server_index() else {
            return;
        };
        let entry = &self.sidebar.servers[srv_idx];
        let server = &entry.name;
        let scope_root = &entry.scope_root;
        let root = &entry.root;
        let messages = self
            .data
            .list_server_message_history(server, scope_root)
            .unwrap_or_default();
        self.popup = Some(ServerPopup::new(server, root, messages));
    }

    /// Close the server message popup.
    pub fn close_popup(&mut self) {
        self.popup = None;
    }
}
