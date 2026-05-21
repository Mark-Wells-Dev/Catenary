// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application state for the TUI.
//!
//! Placeholder for the v2 rewrite. Subsequent tickets build out the
//! unified message stream and sidebar.

use super::data::DataSource;
use super::icons::IconSet;
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
}

impl<'a> App<'a> {
    /// Create a new App with minimal placeholder state.
    ///
    /// # Errors
    ///
    /// Returns an error if listing sessions fails.
    pub fn new(
        theme: &'a Theme,
        icons: &'a IconSet,
        data: Box<dyn DataSource>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            theme,
            icons,
            data,
            quit: false,
            level_threshold: LevelThreshold::Info,
        })
    }
}
