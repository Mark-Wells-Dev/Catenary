// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application state for the TUI.
//!
//! Owns the data source, stream state, and display configuration.
//! The event loop in [`super::run_loop`] drives state transitions.

use super::data::{DataSource, MessageTail};
use super::icons::IconSet;
use super::stream::StreamState;
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
    /// Unified message stream state.
    pub stream: StreamState,
    /// Tail reader for incremental message updates.
    pub tail: Option<Box<dyn MessageTail>>,
}

impl<'a> App<'a> {
    /// Create a new App and load the initial message stream.
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
        let messages = data.monitor_all_messages(include_debug)?;
        let tail = data.create_all_message_tail(include_debug).ok();
        let stream = StreamState::new(messages);

        Ok(Self {
            theme,
            icons,
            data,
            quit: false,
            level_threshold: LevelThreshold::Info,
            stream,
            tail,
        })
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

    /// Reload all messages (e.g., after toggling severity threshold).
    ///
    /// # Errors
    ///
    /// Returns an error if loading messages fails.
    pub fn reload_messages(&mut self) -> anyhow::Result<()> {
        let include_debug = self.level_threshold.include_debug();
        let messages = self.data.monitor_all_messages(include_debug)?;
        self.tail = self.data.create_all_message_tail(include_debug).ok();
        self.stream = StreamState::new(messages);
        Ok(())
    }
}
