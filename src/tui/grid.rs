// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Multi-panel events grid with BSP layout, tab focus cycling, pinning, and
//! layout cycling.
//!
//! The grid is a container for multiple [`PanelState`] instances arranged by
//! the BSP layout engine from [`super::layout`]. It manages panel lifecycle
//! (open/close), `Tab` focus cycling in reading order, `Space` pinning,
//! `w` layout cycling, and `Esc` to clear all pins.

use std::collections::HashSet;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::icons::IconSet;
use super::layout::{Composition, closest_composition, compute_layout, curated_layouts};
use super::panel::{PanelState, render_panel};
use super::theme::Theme;

/// The pin ratio used for pinned panels (pinned panels get 2x space).
const PIN_RATIO: f64 = 2.0;

/// The Events grid containing multiple panels in a BSP layout.
pub struct EventsGrid<'a> {
    /// Open panels, in reading order.
    pub panels: Vec<PanelState<'a>>,
    /// Semantic color theme (borrowed, same lifetime as panels).
    pub theme: &'a Theme,
    /// Resolved icon set (borrowed, same lifetime as panels).
    pub icons: &'a IconSet,
    /// Current BSP composition (panels-per-row).
    pub composition: Composition,
    /// Index of the focused panel (receives keyboard input), if any.
    pub focused: Option<usize>,
    /// Index into `curated_layouts()` for the current panel count.
    /// Used by `w` to cycle.
    pub layout_cycle_index: usize,
}

impl<'a> EventsGrid<'a> {
    /// Create an empty grid with no panels and no focus.
    #[must_use]
    pub const fn new(theme: &'a Theme, icons: &'a IconSet) -> Self {
        Self {
            panels: Vec::new(),
            theme,
            icons,
            composition: Composition(vec![]),
            focused: None,
            layout_cycle_index: 0,
        }
    }

    /// Open a panel for the given session.
    ///
    /// If a panel for this session already exists, returns its index without
    /// creating a duplicate. Otherwise creates a new panel, updates the
    /// composition, and auto-focuses if this is the only panel.
    pub fn open_panel(&mut self, session_id: String) -> usize {
        if let Some(idx) = self.panel_for_session(&session_id) {
            return idx;
        }

        let panel = PanelState::new(session_id, self.theme, self.icons);
        self.panels.push(panel);

        let new_count = self.panels.len();
        self.composition = closest_composition(&self.composition, new_count);
        self.sync_cycle_index();

        if new_count == 1 {
            self.focused = Some(0);
        }

        new_count - 1
    }

    /// Close the panel at the given index.
    ///
    /// Updates the composition for the new panel count and adjusts focus:
    /// - If closing the focused panel: move to the next panel in reading
    ///   order, or the previous if none, or `None` if empty.
    /// - If closing a panel before the focused one: decrement the focus index.
    pub fn close_panel(&mut self, index: usize) {
        if index >= self.panels.len() {
            return;
        }

        self.panels.remove(index);

        let new_count = self.panels.len();
        self.composition = closest_composition(&self.composition, new_count);
        self.sync_cycle_index();

        if new_count == 0 {
            self.focused = None;
            return;
        }

        if let Some(focused) = self.focused {
            if index == focused {
                // Closing the focused panel — move to next (same index if
                // it's still valid, otherwise clamp to last).
                self.focused = Some(index.min(new_count - 1));
            } else if index < focused {
                // Closing before focused — decrement.
                self.focused = Some(focused - 1);
            }
        }
    }

    /// Advance focus to the next panel in reading order (Tab).
    ///
    /// Wraps around to 0 after the last panel. No-op if no panels.
    pub const fn focus_next(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        self.focused = Some(match self.focused {
            Some(idx) => (idx + 1) % self.panels.len(),
            None => 0,
        });
    }

    /// Move focus to the previous panel in reading order (Shift+Tab).
    ///
    /// Wraps to the last panel from 0. No-op if no panels.
    pub const fn focus_prev(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        let len = self.panels.len();
        self.focused = Some(match self.focused {
            Some(0) | None => len - 1,
            Some(idx) => idx - 1,
        });
    }

    /// Set focus to a specific panel by index.
    ///
    /// No-op if the index is out of bounds.
    pub const fn focus_panel(&mut self, index: usize) {
        if index < self.panels.len() {
            self.focused = Some(index);
        }
    }

    /// Toggle pinning on the focused panel (Space).
    ///
    /// No-op if no panel is focused.
    pub fn toggle_pin(&mut self) {
        if let Some(idx) = self.focused
            && let Some(panel) = self.panels.get_mut(idx)
        {
            panel.pinned = !panel.pinned;
        }
    }

    /// Clear all pins (Esc).
    pub fn clear_pins(&mut self) {
        for panel in &mut self.panels {
            panel.pinned = false;
        }
    }

    /// Sync `layout_cycle_index` to the position of the current composition
    /// in the curated list. Falls back to 0 if no match is found.
    fn sync_cycle_index(&mut self) {
        let layouts = curated_layouts(self.panels.len());
        self.layout_cycle_index = layouts
            .iter()
            .position(|c| *c == self.composition)
            .unwrap_or(0);
    }

    /// Cycle to the next curated layout for the current panel count (w).
    ///
    /// No-op if there are no panels or only one layout available.
    pub fn cycle_layout(&mut self) {
        let layouts = curated_layouts(self.panels.len());
        if layouts.is_empty() {
            return;
        }
        self.layout_cycle_index = (self.layout_cycle_index + 1) % layouts.len();
        self.composition = layouts[self.layout_cycle_index].clone();
    }

    /// Get a reference to the focused panel, if any.
    #[must_use]
    pub fn focused_panel(&self) -> Option<&PanelState<'a>> {
        self.focused.and_then(|idx| self.panels.get(idx))
    }

    /// Get a mutable reference to the focused panel, if any.
    pub fn focused_panel_mut(&mut self) -> Option<&mut PanelState<'a>> {
        self.focused.and_then(|idx| self.panels.get_mut(idx))
    }

    /// Collect indices of all pinned panels.
    ///
    /// Passed to [`compute_layout`] for pin-driven sizing.
    #[must_use]
    pub fn pinned_indices(&self) -> HashSet<usize> {
        self.panels
            .iter()
            .enumerate()
            .filter(|(_, p)| p.pinned)
            .map(|(i, _)| i)
            .collect()
    }

    /// Find the panel index for a given session ID, if open.
    #[must_use]
    pub fn panel_for_session(&self, session_id: &str) -> Option<usize> {
        self.panels.iter().position(|p| p.session_id == session_id)
    }
}

/// Render the full events grid into the given buffer area.
///
/// Computes the BSP layout from the grid's composition and pinned set, then
/// renders each panel into its assigned rect. The focused panel receives a
/// highlighted border.
pub fn render_grid(grid: &EventsGrid<'_>, area: Rect, buf: &mut Buffer, show_borders: bool) {
    if grid.panels.is_empty() {
        return;
    }

    let pinned = grid.pinned_indices();
    let layout = compute_layout(area, &grid.composition, &pinned, PIN_RATIO);

    for panel_rect in &layout.panels {
        if panel_rect.index >= grid.panels.len() {
            continue;
        }
        let is_focused = grid.focused == Some(panel_rect.index);
        render_panel(
            &grid.panels[panel_rect.index],
            panel_rect.rect,
            buf,
            is_focused,
            show_borders,
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::IconConfig;
    use crate::session::SessionMessage;

    fn test_theme() -> Theme {
        Theme::new()
    }

    fn test_icons() -> IconSet {
        IconSet::from_config(IconConfig::default())
    }

    fn make_message(method: &str) -> SessionMessage {
        crate::session::test_support::message("lsp", method, "rust-analyzer")
    }

    /// Convert a ratatui buffer to a single string for assertion matching.
    fn buffer_to_string(buf: &Buffer) -> String {
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

    #[test]
    fn test_grid_open_panel() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        let idx = grid.open_panel("session-1".to_string());
        assert_eq!(idx, 0);
        assert_eq!(grid.panels.len(), 1);
        assert_eq!(grid.focused, Some(0));
        assert_eq!(grid.composition.total(), 1);
    }

    #[test]
    fn test_grid_open_duplicate() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("abc".to_string());
        let idx = grid.open_panel("abc".to_string());
        assert_eq!(idx, 0);
        assert_eq!(grid.panels.len(), 1);
    }

    #[test]
    fn test_grid_open_multiple() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());
        assert_eq!(grid.panels.len(), 3);
        assert_eq!(grid.composition.total(), 3);
    }

    #[test]
    fn test_grid_close_panel() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());

        grid.close_panel(1);
        assert_eq!(grid.panels.len(), 2);
        assert_eq!(grid.panels[0].session_id, "s1");
        assert_eq!(grid.panels[1].session_id, "s3");
    }

    #[test]
    fn test_grid_close_focused_moves_focus() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());

        grid.focus_panel(1);
        assert_eq!(grid.focused, Some(1));

        grid.close_panel(1);
        // Focus should move to the next panel (now index 1, was s3).
        assert_eq!(grid.focused, Some(1));
        assert_eq!(grid.panels[1].session_id, "s3");
    }

    #[test]
    fn test_grid_close_before_focused() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());

        grid.focus_panel(2);
        assert_eq!(grid.focused, Some(2));

        grid.close_panel(0);
        assert_eq!(grid.focused, Some(1));
        assert_eq!(grid.panels[1].session_id, "s3");
    }

    #[test]
    fn test_grid_close_last_panel() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.close_panel(0);

        assert!(grid.panels.is_empty());
        assert_eq!(grid.focused, None);
    }

    #[test]
    fn test_grid_focus_next() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());

        grid.focus_panel(0);
        grid.focus_next();
        assert_eq!(grid.focused, Some(1));

        grid.focus_next();
        assert_eq!(grid.focused, Some(2));

        // Wrap around.
        grid.focus_next();
        assert_eq!(grid.focused, Some(0));
    }

    #[test]
    fn test_grid_focus_prev() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());

        grid.focus_panel(0);
        // Wrap to last.
        grid.focus_prev();
        assert_eq!(grid.focused, Some(2));
    }

    #[test]
    fn test_grid_toggle_pin() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());

        grid.focus_panel(0);
        grid.toggle_pin();
        assert!(grid.panels[0].pinned);
        assert!(!grid.panels[1].pinned);
    }

    #[test]
    fn test_grid_clear_pins() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());

        grid.focus_panel(0);
        grid.toggle_pin();
        grid.focus_panel(1);
        grid.toggle_pin();
        assert!(grid.panels[0].pinned);
        assert!(grid.panels[1].pinned);

        grid.clear_pins();
        assert!(!grid.panels[0].pinned);
        assert!(!grid.panels[1].pinned);
    }

    #[test]
    fn test_grid_cycle_layout() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());

        let initial = grid.composition.clone();
        // cycle_index is synced to the curated list, so the first w press
        // always advances to a different composition.
        grid.cycle_layout();
        assert_ne!(
            grid.composition, initial,
            "first cycle should change composition"
        );
    }

    #[test]
    fn test_grid_render_two_panels() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("panel-a".to_string());
        grid.open_panel("panel-b".to_string());

        // Load some messages into each panel.
        grid.panels[0].load_messages(vec![make_message("hover")]);
        grid.panels[1].load_messages(vec![make_message("definition")]);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_grid(&grid, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("panel-a"), "expected panel-a session id");
        assert!(content.contains("panel-b"), "expected panel-b session id");
        assert!(
            content.contains("hover"),
            "expected hover method in panel-a"
        );
        assert!(
            content.contains("definition"),
            "expected definition method in panel-b"
        );
    }

    #[test]
    fn test_grid_pinned_indices() {
        let theme = test_theme();
        let icons = test_icons();
        let mut grid = EventsGrid::new(&theme, &icons);

        grid.open_panel("s1".to_string());
        grid.open_panel("s2".to_string());
        grid.open_panel("s3".to_string());

        grid.focus_panel(0);
        grid.toggle_pin();
        grid.focus_panel(2);
        grid.toggle_pin();

        let pinned = grid.pinned_indices();
        assert!(pinned.contains(&0));
        assert!(!pinned.contains(&1));
        assert!(pinned.contains(&2));
        assert_eq!(pinned.len(), 2);
    }
}
