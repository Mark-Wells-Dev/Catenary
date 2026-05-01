// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application state for the TUI.
//!
//! Contains the core [`App`] struct, [`FocusedPane`] and [`InputMode`] enums,
//! and the constructor that initializes sessions, tree, grid, and focus.

use std::collections::HashMap;

use ratatui::layout::Rect;

use super::data::{DataSource, MessageTail};
use super::filter::FilterState;
use super::grid::EventsGrid;
use super::icons::IconSet;
use super::layout::PanelLayout;
use super::mouse::DragState;
use super::theme::Theme;
use super::tree::{SessionTree, TreeItem};

/// Which region has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusedPane {
    /// The sessions list pane.
    Sessions,
    /// The events detail pane.
    Events,
}

/// Input mode for the TUI run loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    /// Normal navigation mode.
    Normal,
    /// Filter input mode (typing a filter pattern).
    FilterInput,
    /// Visual selection mode.
    Visual,
}

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
}

/// Application state driving the TUI.
pub struct App<'a> {
    /// Semantic color theme.
    pub theme: &'a Theme,
    /// Resolved icon theme.
    pub icons: &'a IconSet,
    /// Data source for session and event data.
    pub data: Box<dyn DataSource>,
    /// Which pane currently has focus.
    pub focus: FocusedPane,
    /// Sessions tree state.
    pub tree: SessionTree,
    /// Events grid state.
    pub grid: EventsGrid<'a>,
    /// Active filter state, if any.
    pub filter: Option<FilterState>,
    /// Current input mode.
    pub input_mode: InputMode,
    /// Whether the Sessions tree is visible.
    pub sessions_visible: bool,
    /// Sessions tree width as a fraction of the terminal.
    pub sessions_width_ratio: f64,
    /// Mouse drag state.
    pub drag_state: DragState,
    /// Whether the user wants to quit.
    pub quit: bool,
    /// Cached tree area (updated each frame).
    pub tree_area: Rect,
    /// Cached grid area (updated each frame).
    pub grid_area: Rect,
    /// Cached panel layout (updated each frame).
    pub grid_layout: Option<PanelLayout>,
    /// Event tails keyed by session ID, for streaming new events into panels.
    pub tails: HashMap<String, Box<dyn MessageTail>>,
    /// Current display level threshold.
    pub level_threshold: LevelThreshold,
    /// Keep panels open after a session dies (from `[tui]` config).
    pub keep_dead_panels: bool,
}

impl<'a> App<'a> {
    /// Create a new App, initializing sessions, tree, and grid.
    ///
    /// Lists sessions from the data source, builds the tree, auto-opens
    /// panels for active sessions, and sets focus on the first active session.
    ///
    /// # Errors
    ///
    /// Returns an error if listing sessions fails.
    pub fn new(
        theme: &'a Theme,
        icons: &'a IconSet,
        data: Box<dyn DataSource>,
        sessions_width_ratio: f64,
        keep_dead_panels: bool,
    ) -> anyhow::Result<Self> {
        let rows = data.list_sessions()?;

        // Collect active session IDs before moving rows into the tree.
        let active_ids: Vec<String> = rows
            .iter()
            .filter(|r| r.alive)
            .map(|r| r.info.id.clone())
            .collect();

        let mut tree = SessionTree::from_sessions(rows);

        let mut grid = EventsGrid::new(theme, icons);

        // Auto-open panels for active sessions.
        for id in &active_ids {
            grid.open_panel(id.clone());
        }

        // Set cursor on first active session in the tree.
        let first_active =
            tree.visible_items()
                .iter()
                .enumerate()
                .find_map(|(i, item)| match item {
                    TreeItem::Session { row, .. } if row.alive => Some(i),
                    _ => None,
                });

        if let Some(cursor) = first_active {
            tree.cursor = cursor;
        }

        Ok(Self {
            theme,
            icons,
            data,
            focus: FocusedPane::Sessions,
            tree,
            grid,
            filter: None,
            input_mode: InputMode::Normal,
            sessions_visible: true,
            sessions_width_ratio,
            drag_state: DragState::Idle,
            quit: false,
            tree_area: Rect::default(),
            grid_area: Rect::default(),
            grid_layout: None,
            tails: HashMap::new(),
            level_threshold: LevelThreshold::Info,
            keep_dead_panels,
        })
    }
}
