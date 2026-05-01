// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Mouse event routing: click, scroll, and drag handling.
//!
//! This module resolves raw mouse coordinates into semantic [`MouseAction`]s
//! by hit-testing against the layout. The caller (run loop / input handler)
//! dispatches actions to the appropriate module (grid, panel, tree, etc.).

use ratatui::layout::Rect;

use super::layout::{PanelLayout, PanelRect, PanelZone, panel_zone_at};
use super::scrollbar::{OverflowCounts, OverflowHit, overflow_hit_test};

// ── Types ───────────────────────────────────────────────────────────────

/// Resolved mouse action after hit-testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseAction {
    /// Focus an Events panel (by index in the grid).
    FocusPanel(usize),
    /// Focus the Sessions tree.
    FocusTree,
    /// Toggle expansion on an event in a panel.
    ToggleExpansion {
        /// Panel index in the grid.
        panel: usize,
        /// Line index in the panel's flat lines.
        line: usize,
    },
    /// Select a session in the tree.
    SelectSession {
        /// Index in the visible tree items.
        item: usize,
    },
    /// Toggle pin on a panel (title click).
    TogglePin(usize),
    /// Scroll a panel's viewport (mouse wheel).
    ScrollPanel {
        /// Panel index in the grid.
        panel: usize,
        /// Scroll delta (positive = down, negative = up).
        delta: isize,
    },
    /// Scroll the Sessions tree viewport (mouse wheel).
    ScrollTree(isize),
    /// Start dragging the Sessions/Events border.
    StartBorderDrag {
        /// Current x position of the border.
        x: u16,
    },
    /// Continue dragging the Sessions/Events border.
    ContinueBorderDrag {
        /// New x position during drag.
        x: u16,
    },
    /// End border drag.
    EndBorderDrag,
    /// Start drag-selecting event lines in a panel.
    StartDragSelect {
        /// Panel index in the grid.
        panel: usize,
        /// Starting flat-line index.
        line: usize,
    },
    /// Continue drag selection.
    ContinueDragSelect {
        /// Panel index in the grid.
        panel: usize,
        /// Current flat-line index.
        line: usize,
    },
    /// Start dragging a scrollbar thumb.
    StartScrollbarDrag {
        /// Panel index in the grid.
        panel: usize,
        /// Y position of the click.
        y: u16,
    },
    /// Continue scrollbar drag.
    ContinueScrollbarDrag {
        /// Panel index in the grid.
        panel: usize,
        /// Current y position.
        y: u16,
    },
    /// Click on overflow count indicator — viewport-only jump.
    /// Moves `scroll_offset` to top or bottom without changing cursor.
    JumpOverflow {
        /// Panel index in the grid.
        panel: usize,
        /// true = top (▲), false = bottom (▼).
        top: bool,
    },
    /// No actionable target (click on empty space, border, etc.).
    None,
}

/// Tracks ongoing drag state across mouse move events.
#[derive(Debug, Clone)]
pub enum DragState {
    /// Not dragging.
    Idle,
    /// Dragging the Sessions/Events border.
    BorderResize {
        /// X position where the drag started.
        initial_x: u16,
    },
    /// Drag-selecting lines in a panel.
    LineSelect {
        /// Panel index.
        panel: usize,
        /// Anchor flat-line index (where the drag started).
        anchor: usize,
    },
    /// Dragging a scrollbar thumb.
    Scrollbar {
        /// Panel index.
        panel: usize,
    },
}

// ── Hit-testing functions ───────────────────────────────────────────────

/// Hit-test a mouse click against the layout.
///
/// Checks, in order: tree area, Sessions/Events border, panel title bar,
/// panel scrollbar (right column), panel content interior (with overflow
/// indicator check), and falls through to `None`.
#[must_use]
#[allow(
    clippy::too_many_arguments,
    reason = "hit-test context is naturally wide"
)]
pub fn resolve_click(
    x: u16,
    y: u16,
    tree_area: Rect,
    grid_layout: &PanelLayout,
    sessions_events_border_x: u16,
    tree_scroll_offset: usize,
    panel_scroll_offsets: &[usize],
    overflow_counts: &[OverflowCounts],
    show_borders: bool,
) -> MouseAction {
    // Check tree area.
    if x >= tree_area.x
        && x < tree_area.x + tree_area.width
        && y >= tree_area.y
        && y < tree_area.y + tree_area.height
    {
        // Content starts at tree_area.y + 1 (after top border).
        let content_y = tree_area.y + 1;
        if y >= content_y {
            let item = (y - content_y) as usize + tree_scroll_offset;
            return MouseAction::SelectSession { item };
        }
        return MouseAction::FocusTree;
    }

    // Check Sessions/Events border (±1 pixel tolerance).
    if x >= sessions_events_border_x.saturating_sub(1)
        && x <= sessions_events_border_x.saturating_add(1)
    {
        return MouseAction::StartBorderDrag {
            x: sessions_events_border_x,
        };
    }

    // Single-pass panel zone hit-test.
    if let Some((panel_idx, zone)) = panel_zone_at(grid_layout, x, y, show_borders) {
        let panel_rect = &grid_layout.panels[panel_idx];
        return match zone {
            PanelZone::TitleBar => MouseAction::TogglePin(panel_idx),
            PanelZone::Scrollbar => MouseAction::StartScrollbarDrag {
                panel: panel_idx,
                y,
            },
            PanelZone::Content => {
                // Check overflow indicators first.
                let content_area = content_area_of(panel_rect, show_borders);
                if let Some(counts) = overflow_counts.get(panel_idx)
                    && let Some(hit) = overflow_hit_test(x, y, content_area, counts)
                {
                    return MouseAction::JumpOverflow {
                        panel: panel_idx,
                        top: hit == OverflowHit::Top,
                    };
                }
                // Regular content click — compute flat-line index.
                let scroll_offset = panel_scroll_offsets.get(panel_idx).copied().unwrap_or(0);
                let line = compute_line_from_click(y, panel_rect, scroll_offset, show_borders);
                MouseAction::ToggleExpansion {
                    panel: panel_idx,
                    line,
                }
            }
        };
    }

    MouseAction::None
}

/// Route a scroll event to the correct panel or tree.
///
/// Scroll does NOT change focus — it only returns `ScrollPanel` or
/// `ScrollTree`.
#[must_use]
pub fn resolve_scroll(
    x: u16,
    y: u16,
    delta: isize,
    tree_area: Rect,
    grid_layout: &PanelLayout,
) -> MouseAction {
    // Check tree area.
    if x >= tree_area.x
        && x < tree_area.x + tree_area.width
        && y >= tree_area.y
        && y < tree_area.y + tree_area.height
    {
        return MouseAction::ScrollTree(delta);
    }

    // Check panels (including chrome — scroll should work on title bar and
    // scrollbar column too).
    for panel_rect in &grid_layout.panels {
        let r = &panel_rect.rect;
        if x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height {
            return MouseAction::ScrollPanel {
                panel: panel_rect.index,
                delta,
            };
        }
    }

    MouseAction::None
}

/// Resolve a drag (mouse move with button held) based on current drag state.
#[must_use]
pub fn resolve_drag(
    x: u16,
    y: u16,
    drag_state: &DragState,
    grid_layout: &PanelLayout,
    panel_scroll_offsets: &[usize],
    show_borders: bool,
) -> MouseAction {
    match drag_state {
        DragState::BorderResize { .. } => MouseAction::ContinueBorderDrag { x },
        DragState::LineSelect { panel, .. } => {
            // Compute line from y for the panel.
            grid_layout
                .panels
                .get(*panel)
                .map_or(MouseAction::None, |panel_rect| {
                    let scroll_offset = panel_scroll_offsets.get(*panel).copied().unwrap_or(0);
                    let line = compute_line_from_click(y, panel_rect, scroll_offset, show_borders);
                    MouseAction::ContinueDragSelect {
                        panel: *panel,
                        line,
                    }
                })
        }
        DragState::Scrollbar { panel } => MouseAction::ContinueScrollbarDrag { panel: *panel, y },
        DragState::Idle => MouseAction::None,
    }
}

/// Resolve a mouse button release.
///
/// Returns `EndBorderDrag` for border resizes; `None` for everything else
/// (drag selection and scrollbar drags end implicitly).
#[must_use]
pub const fn resolve_release(drag_state: &DragState) -> MouseAction {
    match drag_state {
        DragState::BorderResize { .. } => MouseAction::EndBorderDrag,
        _ => MouseAction::None,
    }
}

/// Convert a click y-coordinate into a flat-line index for the panel.
///
/// When borders are shown, content starts at `panel_rect.rect.y + 1`
/// (below the title bar). When hidden, content starts at `rect.y`.
/// The returned index accounts for the panel's scroll offset.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub const fn compute_line_from_click(
    y: u16,
    panel_rect: &PanelRect,
    scroll_offset: usize,
    show_borders: bool,
) -> usize {
    let chrome = if show_borders { 1 } else { 0 };
    let content_top = panel_rect.rect.y + chrome;
    let row_in_viewport = y.saturating_sub(content_top) as usize;
    scroll_offset + row_in_viewport
}

/// Compute the new Sessions panel width from a drag x position.
///
/// Returns `None` if `x` is below `min_width` (collapse trigger).
/// Clamps the result to `[min_width, terminal_width - min_width]`.
#[must_use]
pub fn compute_sessions_width_from_drag(
    x: u16,
    terminal_width: u16,
    min_width: u16,
) -> Option<u16> {
    if x < min_width {
        return None; // Collapse trigger.
    }
    let max_width = terminal_width.saturating_sub(min_width);
    Some(x.clamp(min_width, max_width))
}

/// Compute the content area of a panel (excluding title bar and scrollbar).
const fn content_area_of(panel_rect: &PanelRect, show_borders: bool) -> Rect {
    let r = &panel_rect.rect;
    let chrome = if show_borders { 1 } else { 0 };
    Rect::new(
        r.x,
        r.y + chrome,
        r.width.saturating_sub(1),
        r.height.saturating_sub(chrome),
    )
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::tui::layout::{Composition, compute_layout};

    /// Build a simple single-panel grid layout for testing.
    fn single_panel_layout(area: Rect) -> PanelLayout {
        let comp = Composition(vec![1]);
        compute_layout(area, &comp, &HashSet::new(), 1.0)
    }

    /// Build a two-panel side-by-side grid layout for testing.
    fn two_panel_layout(area: Rect) -> PanelLayout {
        let comp = Composition(vec![2]);
        compute_layout(area, &comp, &HashSet::new(), 1.0)
    }

    fn no_overflow() -> Vec<OverflowCounts> {
        vec![OverflowCounts { above: 0, below: 0 }]
    }

    fn no_overflow_n(n: usize) -> Vec<OverflowCounts> {
        vec![OverflowCounts { above: 0, below: 0 }; n]
    }

    #[test]
    fn test_click_in_tree_area() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(31, 0, 49, 20);
        let layout = single_panel_layout(grid_area);

        let action = resolve_click(5, 3, tree_area, &layout, 30, 0, &[], &no_overflow(), true);
        // y=3, content_y=1, so item = 3 - 1 + 0 = 2
        assert_eq!(action, MouseAction::SelectSession { item: 2 });
    }

    #[test]
    fn test_click_in_panel_content() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        // Click inside panel 0's content area (below title, left of scrollbar).
        let panel_r = &layout.panels[0].rect;
        let cx = panel_r.x + 5;
        let cy = panel_r.y + 3; // below title row

        let action = resolve_click(
            cx,
            cy,
            tree_area,
            &layout,
            31,
            0,
            &[0],
            &no_overflow(),
            true,
        );
        assert_eq!(action, MouseAction::ToggleExpansion { panel: 0, line: 2 });
    }

    #[test]
    fn test_click_in_scrolled_panel() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        let panel_r = &layout.panels[0].rect;
        let cx = panel_r.x + 5;
        let cy = panel_r.y + 3; // below title row → viewport row 2

        // Panel scrolled down by 50 lines — click should resolve to line 52.
        let action = resolve_click(
            cx,
            cy,
            tree_area,
            &layout,
            31,
            0,
            &[50],
            &no_overflow(),
            true,
        );
        assert_eq!(action, MouseAction::ToggleExpansion { panel: 0, line: 52 });
    }

    #[test]
    fn test_click_in_scrolled_tree() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(31, 0, 49, 20);
        let layout = single_panel_layout(grid_area);

        // Tree scrolled down by 10 — click at y=3 should resolve to item 12.
        let action = resolve_click(5, 3, tree_area, &layout, 30, 10, &[], &no_overflow(), true);
        // y=3, content_y=1, so item = (3 - 1) + 10 = 12
        assert_eq!(action, MouseAction::SelectSession { item: 12 });
    }

    #[test]
    fn test_click_on_panel_title() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = two_panel_layout(grid_area);

        let panel1_r = &layout.panels[1].rect;
        // Click on the title row of panel 1.
        let cx = panel1_r.x + 3;
        let cy = panel1_r.y;

        let action = resolve_click(
            cx,
            cy,
            tree_area,
            &layout,
            31,
            0,
            &[0, 0],
            &no_overflow_n(2),
            true,
        );
        assert_eq!(action, MouseAction::TogglePin(1));
    }

    #[test]
    fn test_click_on_border() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);
        let border_x = 30u16;

        let action = resolve_click(
            30,
            5,
            tree_area,
            &layout,
            border_x,
            0,
            &[],
            &no_overflow(),
            true,
        );
        assert_eq!(action, MouseAction::StartBorderDrag { x: 30 });
    }

    #[test]
    fn test_click_outside_all() {
        // Tree at left, grid at right, click way off.
        let tree_area = Rect::new(0, 0, 10, 10);
        let grid_area = Rect::new(15, 0, 20, 10);
        let layout = single_panel_layout(grid_area);

        // Click at (12, 5) — between tree and grid, not on border either.
        // Border at x=11, so ±1 is 10..=12. Let's use border_x=11.
        // Actually, let's make border far away to test a true miss.
        let action = resolve_click(80, 80, tree_area, &layout, 11, 0, &[], &no_overflow(), true);
        assert_eq!(action, MouseAction::None);
    }

    #[test]
    fn test_scroll_in_panel() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        let panel_r = &layout.panels[0].rect;
        let cx = panel_r.x + 5;
        let cy = panel_r.y + 5;

        let action = resolve_scroll(cx, cy, 3, tree_area, &layout);
        assert_eq!(action, MouseAction::ScrollPanel { panel: 0, delta: 3 });
    }

    #[test]
    fn test_scroll_in_tree() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        let action = resolve_scroll(10, 5, -2, tree_area, &layout);
        assert_eq!(action, MouseAction::ScrollTree(-2));
    }

    #[test]
    fn test_scroll_does_not_focus() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        // Scroll in panel.
        let panel_r = &layout.panels[0].rect;
        let action = resolve_scroll(panel_r.x + 5, panel_r.y + 5, 1, tree_area, &layout);
        assert!(
            !matches!(action, MouseAction::FocusPanel(_)),
            "scroll should never return FocusPanel"
        );

        // Scroll in tree.
        let action = resolve_scroll(5, 5, 1, tree_area, &layout);
        assert!(
            !matches!(action, MouseAction::FocusPanel(_)),
            "scroll should never return FocusPanel"
        );
    }

    #[test]
    fn test_drag_border() {
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        let drag = DragState::BorderResize { initial_x: 30 };
        let action = resolve_drag(20, 5, &drag, &layout, &[], true);
        assert_eq!(action, MouseAction::ContinueBorderDrag { x: 20 });
    }

    #[test]
    fn test_compute_line_from_click() {
        // Panel at y=5, height 20, title row at y=5, content starts at y=6.
        let panel_rect = PanelRect {
            rect: Rect::new(0, 5, 40, 20),
            row: 0,
            col: 0,
            index: 0,
        };
        // Click at y=10, scroll_offset=5.
        // row_in_viewport = 10 - 6 = 4
        // line = 5 + 4 = 9
        let line = compute_line_from_click(10, &panel_rect, 5, true);
        assert_eq!(line, 9);
    }

    #[test]
    fn test_compute_sessions_width_collapse() {
        let result = compute_sessions_width_from_drag(5, 100, 10);
        assert_eq!(result, None, "below min_width should trigger collapse");
    }

    #[test]
    fn test_compute_sessions_width_normal() {
        let result = compute_sessions_width_from_drag(40, 100, 10);
        assert_eq!(result, Some(40));
    }

    #[test]
    fn test_click_overflow_top() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        let content = content_area_of(&layout.panels[0], true);

        // Overflow: 15 lines above → " 15▲" rendered right-aligned.
        // "15▲" label is 3 columns wide, right-aligned in content area.
        let counts = vec![OverflowCounts {
            above: 15,
            below: 0,
        }];

        // Click on the digits of the top indicator.
        let right = content.x + content.width;
        let label_x = right - 3; // "15▲" is 3 wide
        let cy = content.y; // first content row = top indicator row

        let action = resolve_click(label_x, cy, tree_area, &layout, 31, 0, &[0], &counts, true);
        assert_eq!(
            action,
            MouseAction::JumpOverflow {
                panel: 0,
                top: true
            }
        );
    }

    #[test]
    fn test_click_overflow_bottom() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        let content = content_area_of(&layout.panels[0], true);

        let counts = vec![OverflowCounts {
            above: 0,
            below: 10,
        }];

        // Click on the bottom indicator.
        let right = content.x + content.width;
        let label_x = right - 3; // "10▼" is 3 wide
        let cy = content.y + content.height - 1; // last content row

        let action = resolve_click(label_x, cy, tree_area, &layout, 31, 0, &[0], &counts, true);
        assert_eq!(
            action,
            MouseAction::JumpOverflow {
                panel: 0,
                top: false
            }
        );
    }

    #[test]
    fn test_click_overflow_padding_miss() {
        let tree_area = Rect::new(0, 0, 30, 20);
        let grid_area = Rect::new(32, 0, 48, 20);
        let layout = single_panel_layout(grid_area);

        let content = content_area_of(&layout.panels[0], true);

        let counts = vec![OverflowCounts {
            above: 15,
            below: 0,
        }];

        // Click on the leading space before the indicator.
        // " 15▲" is 4 wide total, "15▲" label is 3 wide.
        // The leading space is at (right - 4), which is outside the label hit zone.
        let right = content.x + content.width;
        let space_x = right - 4; // the leading space
        let cy = content.y;

        let action = resolve_click(space_x, cy, tree_area, &layout, 31, 0, &[0], &counts, true);
        // Should NOT be JumpOverflow — falls through to ToggleExpansion.
        assert!(
            !matches!(action, MouseAction::JumpOverflow { .. }),
            "click on padding space should not trigger JumpOverflow, got {action:?}"
        );
    }
}
