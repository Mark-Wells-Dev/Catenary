// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Terminal theme for the TUI.
//!
//! All colors use the terminal's ANSI palette so the TUI automatically
//! inherits whatever theme the user has configured.

use ratatui::style::{Color, Modifier, Style};

// ── Theme ────────────────────────────────────────────────────────────────

/// Semantic color theme that defers to the terminal's ANSI palette.
///
/// Uses only base ANSI colors (`Color::Green`, `Color::Red`, etc.) and
/// modifiers (`BOLD`, `DIM`) so the TUI automatically inherits whatever theme
/// the user has configured in their terminal emulator. It never reverse-videos
/// or hard-codes a background — grays must stay grays on both light and dark
/// terminals (tui-rework 11, item 3).
pub struct Theme {
    /// Style for the focused pane border (bold frame — the "bounding box").
    pub border_focused: Style,
    /// Style for the unfocused pane border (plain).
    pub border_unfocused: Style,
    /// Style for pane titles.
    pub title: Style,
    /// Style for hint keybinding labels.
    pub hint_key: Style,
    /// Style for hint description text.
    pub hint_label: Style,
    /// Style for the selection highlight.
    pub selection: Style,
    /// Style for visual selection highlight (distinct from cursor).
    pub visual_selection: Style,

    /// Style for active sessions.
    pub session_active: Style,
    /// Style for dead sessions.
    pub session_dead: Style,
    /// Style for session metadata (language list, etc.).
    pub session_meta: Style,

    /// Style for timestamps.
    pub timestamp: Style,
    /// Style for normal text.
    pub text: Style,
    /// Style for accented text (language names, etc.).
    pub accent: Style,
    /// Style for success indicators.
    pub success: Style,
    /// Style for error indicators.
    pub error: Style,
    /// Style for warning indicators.
    pub warning: Style,
    /// Style for informational indicators.
    pub info: Style,
    /// Style for muted/dimmed text.
    pub muted: Style,

    /// Style for display rows matching the current search query.
    pub search_match: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new()
    }
}

impl Theme {
    /// Build the default theme from the terminal's palette.
    ///
    /// Focus and selection are shown with the `BOLD` modifier — never a
    /// reverse-video or background swap — so grays stay legible on both light
    /// and dark terminals (tui-rework 11, item 3).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            border_focused: Style::new().add_modifier(Modifier::BOLD),
            border_unfocused: Style::new(),
            title: Style::new().add_modifier(Modifier::BOLD),
            hint_key: Style::new().add_modifier(Modifier::BOLD),
            hint_label: Style::new().add_modifier(Modifier::DIM),
            selection: Style::new().add_modifier(Modifier::BOLD),
            visual_selection: Style::new().bg(Color::Yellow),

            session_active: Style::new().fg(Color::Green),
            session_dead: Style::new().add_modifier(Modifier::DIM),
            session_meta: Style::new().add_modifier(Modifier::DIM),

            timestamp: Style::new().add_modifier(Modifier::DIM),
            text: Style::new(),
            accent: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            error: Style::new().fg(Color::Red),
            warning: Style::new().fg(Color::Yellow),
            info: Style::new().fg(Color::Blue),
            muted: Style::new().add_modifier(Modifier::DIM),

            search_match: Style::new().fg(Color::Black).bg(Color::Yellow),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_construction() {
        let theme = Theme::new();
        // Focus is a bold frame; the unfocused border stays plain (item 3).
        assert!(theme.border_focused.add_modifier.contains(Modifier::BOLD));
        assert!(theme.border_unfocused.add_modifier.is_empty());
        // Selection is bold, never reverse-video (palette honesty).
        assert!(theme.selection.add_modifier.contains(Modifier::BOLD));
        assert!(!theme.selection.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(theme.selection.bg, None, "no background swap on selection");
        assert_eq!(theme.selection.fg, None, "no foreground swap on selection");
    }
}
