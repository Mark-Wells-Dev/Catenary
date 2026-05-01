// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! BSP layout engine and border junction character lookup.
//!
//! Pure geometry — no terminal I/O or rendering framework coupling.
//! Divides a [`Rect`] into non-overlapping [`PanelRect`]s according to a
//! [`Composition`], with optional pin-driven sizing.
//!
//! ## Border ownership
//!
//! Each panel owns its **top** row (title bar) and **right** column
//! (scrollbar). A panel does NOT own its left or bottom edges — the left
//! edge is the scrollbar of the neighbor to the left (or nothing if
//! leftmost), and the bottom edge is the title bar of the panel below
//! (or the hints/filter row).
//!
//! The [`box_char`] function provides a Unicode box-drawing character
//! lookup from arm flags. Rendering code builds the flags based on which
//! title bars and scrollbars meet at each junction point.

use std::collections::HashSet;
use std::hash::BuildHasher;

use ratatui::layout::Rect;

// ── Types ───────────────────────────────────────────────────────────────

/// An ordered partition of N panels into rows.
///
/// Each element is the number of panels in that row.
/// Example: `[2, 1]` means 2 panels in row 0, 1 panel in row 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition(pub Vec<usize>);

impl Composition {
    /// Total number of panels across all rows.
    #[must_use]
    pub fn total(&self) -> usize {
        self.0.iter().sum()
    }
}

/// A positioned panel within the grid.
#[derive(Debug, Clone)]
pub struct PanelRect {
    /// The screen area for this panel (non-overlapping, includes owned
    /// top border row and right scrollbar column).
    pub rect: Rect,
    /// Which row this panel is in (0-indexed).
    pub row: usize,
    /// Which column within the row (0-indexed).
    pub col: usize,
    /// Global panel index (0-indexed, reading order).
    pub index: usize,
}

/// The computed layout of all panels.
#[derive(Debug)]
pub struct PanelLayout {
    /// Positioned panels in reading order.
    pub panels: Vec<PanelRect>,
}

// ── Layout computation ──────────────────────────────────────────────────

/// Compute the layout of panels within the given area.
///
/// Produces non-overlapping rects that tile the area exactly (sum of
/// widths per row = area width, sum of heights across rows = area height).
/// Pinned panels receive proportionally more space according to `pin_ratio`.
/// Remainder pixels are distributed left-to-right, top-to-bottom.
#[must_use]
pub fn compute_layout<S: BuildHasher>(
    area: Rect,
    composition: &Composition,
    pinned: &HashSet<usize, S>,
    pin_ratio: f64,
) -> PanelLayout {
    let num_rows = composition.0.len();
    if num_rows == 0 || area.height == 0 || area.width == 0 {
        return PanelLayout { panels: Vec::new() };
    }

    // Determine which rows contain a pinned panel.
    let mut global_idx = 0usize;
    let row_has_pin: Vec<bool> = composition
        .0
        .iter()
        .map(|&cols| {
            let has = (global_idx..global_idx + cols).any(|i| pinned.contains(&i));
            global_idx += cols;
            has
        })
        .collect();

    // Compute row heights with pin scaling.
    let row_weights: Vec<f64> = row_has_pin
        .iter()
        .map(|&has_pin| if has_pin { pin_ratio } else { 1.0 })
        .collect();
    let row_sizes = distribute_weighted(area.height, &row_weights);

    // Build panels row by row.
    let mut panels = Vec::with_capacity(composition.total());
    let mut y = area.y;
    let mut panel_index = 0usize;

    for (row_idx, &cols_in_row) in composition.0.iter().enumerate() {
        let h = row_sizes[row_idx];

        // Column weights for this row.
        let col_weights: Vec<f64> = (0..cols_in_row)
            .map(|c| {
                if pinned.contains(&(panel_index + c)) {
                    pin_ratio
                } else {
                    1.0
                }
            })
            .collect();

        let col_widths = distribute_weighted(area.width, &col_weights);

        let mut x = area.x;
        for (col_idx, &w) in col_widths.iter().enumerate() {
            panels.push(PanelRect {
                rect: Rect::new(x, y, w, h),
                row: row_idx,
                col: col_idx,
                index: panel_index,
            });
            x += w;
            panel_index += 1;
        }

        y += h;
    }

    PanelLayout { panels }
}

/// Distribute `total` pixels among `n` slots with the given weights.
///
/// Each slot gets at least `floor(total * weight / sum_weights)` pixels.
/// Remainder pixels are distributed to slots in order (left-to-right or
/// top-to-bottom) by largest fractional part, breaking ties by index.
fn distribute_weighted(total: u16, weights: &[f64]) -> Vec<u16> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }

    let sum: f64 = weights.iter().sum();
    if sum <= 0.0 {
        // All zero weights — distribute equally.
        return distribute_weighted(total, &vec![1.0; n]);
    }

    let mut sizes = Vec::with_capacity(n);
    let mut remainders = Vec::with_capacity(n);
    let mut allocated = 0u16;

    for (i, &w) in weights.iter().enumerate() {
        let exact = f64::from(total) * w / sum;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "exact is non-negative and bounded by u16::MAX"
        )]
        let floor = exact.floor() as u16;
        sizes.push(floor);
        remainders.push((exact - f64::from(floor), i));
        allocated += floor;
    }

    // Distribute leftover pixels by largest fractional remainder.
    let mut leftover = total - allocated;
    remainders.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for &(_, idx) in &remainders {
        if leftover == 0 {
            break;
        }
        sizes[idx] += 1;
        leftover -= 1;
    }

    sizes
}

/// Integer square root rounded up (pure integer math).
const fn isqrt_ceil(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let s = n.isqrt();
    if s * s == n { s } else { s + 1 }
}

// ── Curated layouts ─────────────────────────────────────────────────────

/// Return a curated subset of practical compositions for `n` panels.
///
/// The `w` key cycles through this list.
#[must_use]
pub fn curated_layouts(n: usize) -> Vec<Composition> {
    match n {
        0 => vec![],
        1 => vec![Composition(vec![1])],
        2 => vec![Composition(vec![1, 1]), Composition(vec![2])],
        3 => vec![
            Composition(vec![1, 1, 1]),
            Composition(vec![3]),
            Composition(vec![2, 1]),
            Composition(vec![1, 2]),
        ],
        4 => vec![
            Composition(vec![2, 2]),
            Composition(vec![1, 1, 1, 1]),
            Composition(vec![4]),
            Composition(vec![2, 1, 1]),
            Composition(vec![1, 1, 2]),
            Composition(vec![3, 1]),
            Composition(vec![1, 3]),
        ],
        _ => {
            let mut layouts = Vec::new();
            // Balanced grid: rows of isqrt(n), last row gets remainder.
            let cols = isqrt_ceil(n);
            let full_rows = n / cols;
            let remainder = n % cols;
            let mut balanced = vec![cols; full_rows];
            if remainder > 0 {
                balanced.push(remainder);
            }
            layouts.push(Composition(balanced));
            // Single column.
            layouts.push(Composition(vec![1; n]));
            // Single row.
            layouts.push(Composition(vec![n]));
            // Top-heavy: (ceil(n/2), floor(n/2)).
            let top = n.div_ceil(2);
            let bottom = n - top;
            if bottom > 0 {
                layouts.push(Composition(vec![top, bottom]));
            }
            // Bottom-heavy: (floor(n/2), ceil(n/2)).
            let top2 = n / 2;
            let bottom2 = n - top2;
            if top2 > 0 && vec![top2, bottom2] != vec![top, bottom] {
                layouts.push(Composition(vec![top2, bottom2]));
            }
            layouts
        }
    }
}

// ── Closest composition ─────────────────────────────────────────────────

/// When a panel is opened or closed, pick the best composition for `new_n`
/// that preserves the shape of `old`.
#[must_use]
pub fn closest_composition(old: &Composition, new_n: usize) -> Composition {
    if new_n == 0 {
        return Composition(vec![]);
    }

    let old_total = old.total();
    if new_n == old_total {
        return old.clone();
    }

    let mut rows = old.0.clone();

    if new_n < old_total {
        // Remove panels from the last row first.
        let mut to_remove = old_total - new_n;
        while to_remove > 0 {
            let Some(last) = rows.last_mut() else {
                break;
            };
            if *last <= to_remove {
                to_remove -= *last;
                rows.pop();
            } else {
                *last -= to_remove;
                to_remove = 0;
            }
        }
        if rows.is_empty() {
            return curated_layouts(new_n)
                .into_iter()
                .next()
                .unwrap_or_else(|| Composition(vec![new_n]));
        }
        return Composition(rows);
    }

    // new_n > old_total: add to the last row or append a new row.
    let to_add = new_n - old_total;
    // Heuristic: if the last row would become very wide, start a new row.
    let max_row = rows.iter().copied().max().unwrap_or(1);
    if let Some(last) = rows.last_mut()
        && *last + to_add <= max_row + 1
    {
        *last += to_add;
        return Composition(rows);
    }
    rows.push(to_add);
    Composition(rows)
}

// ── Box-drawing character lookup ────────────────────────────────────────

/// Bit flags for junction arm presence.
pub const ARM_UP: u8 = 0b0000_0001;
/// Bit flag: arm extending downward.
pub const ARM_DOWN: u8 = 0b0000_0010;
/// Bit flag: arm extending left.
pub const ARM_LEFT: u8 = 0b0000_0100;
/// Bit flag: arm extending right.
pub const ARM_RIGHT: u8 = 0b0000_1000;
/// Bit flag: up arm is heavy (focused).
pub const HEAVY_UP: u8 = 0b0001_0000;
/// Bit flag: down arm is heavy (focused).
pub const HEAVY_DOWN: u8 = 0b0010_0000;
/// Bit flag: left arm is heavy (focused).
pub const HEAVY_LEFT: u8 = 0b0100_0000;
/// Bit flag: right arm is heavy (focused).
pub const HEAVY_RIGHT: u8 = 0b1000_0000;

/// Look up the Unicode box-drawing character for a junction point.
///
/// `flags` is a bitfield combining arm presence (`ARM_*`) in the low
/// nibble and heavy/light weight (`HEAVY_*`) in the high nibble.
/// Rendering code builds the flags based on which title bars (horizontal)
/// and scrollbars (vertical) meet at each junction point, then calls this
/// function to get the correct character.
///
/// Returns a space if no arms are present.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "exhaustive match on junction flag combinations"
)]
pub const fn box_char(flags: u8) -> char {
    let arms = flags & 0x0F;
    let heavy = flags >> 4;

    // For each arm that is present, check if it's heavy.
    let hu = arms & ARM_UP != 0 && heavy & 0x01 != 0;
    let hd = arms & ARM_DOWN != 0 && heavy & 0x02 != 0;
    let hl = arms & ARM_LEFT != 0 && heavy & 0x04 != 0;
    let hr = arms & ARM_RIGHT != 0 && heavy & 0x08 != 0;

    let up = arms & ARM_UP != 0;
    let down = arms & ARM_DOWN != 0;
    let left = arms & ARM_LEFT != 0;
    let right = arms & ARM_RIGHT != 0;

    match (up, down, left, right) {
        // Straight segments.
        (true, true, false, false) => {
            if hu || hd {
                '┃'
            } else {
                '│'
            }
        }
        (false, false, true, true) => {
            if hl || hr {
                '━'
            } else {
                '─'
            }
        }

        // Corners.
        (false, true, false, true) => match (hd, hr) {
            (false, false) => '┌',
            (true, true) => '┏',
            (true, false) => '┎',
            (false, true) => '┍',
        },
        (false, true, true, false) => match (hd, hl) {
            (false, false) => '┐',
            (true, true) => '┓',
            (true, false) => '┒',
            (false, true) => '┑',
        },
        (true, false, false, true) => match (hu, hr) {
            (false, false) => '└',
            (true, true) => '┗',
            (true, false) => '┖',
            (false, true) => '┕',
        },
        (true, false, true, false) => match (hu, hl) {
            (false, false) => '┘',
            (true, true) => '┛',
            (true, false) => '┚',
            (false, true) => '┙',
        },

        // T-junctions.
        (true, true, false, true) => {
            // ├ variants
            match (hu, hd, hr) {
                (false, false, false) => '├',
                (true, true, true) => '┣',
                (true, true, false) => '┠',
                (false, false, true) => '┝',
                (true, false, false) => '┞',
                (false, true, false) => '┟',
                (true, false, true) => '┡',
                (false, true, true) => '┢',
            }
        }
        (true, true, true, false) => {
            // ┤ variants
            match (hu, hd, hl) {
                (false, false, false) => '┤',
                (true, true, true) => '┫',
                (true, true, false) => '┨',
                (false, false, true) => '┥',
                (true, false, false) => '┦',
                (false, true, false) => '┧',
                (true, false, true) => '┩',
                (false, true, true) => '┪',
            }
        }
        (false, true, true, true) => {
            // ┬ variants
            match (hd, hl, hr) {
                (false, false, false) => '┬',
                (true, true, true) => '┳',
                (true, false, false) => '┰',
                (false, true, true) => '┯',
                (false, true, false) => '┭',
                (false, false, true) => '┮',
                (true, true, false) => '┱',
                (true, false, true) => '┲',
            }
        }
        (true, false, true, true) => {
            // ┴ variants
            match (hu, hl, hr) {
                (false, false, false) => '┴',
                (true, true, true) => '┻',
                (true, false, false) => '┸',
                (false, true, true) => '┷',
                (false, true, false) => '┵',
                (false, false, true) => '┶',
                (true, true, false) => '┹',
                (true, false, true) => '┺',
            }
        }

        // Cross.
        (true, true, true, true) => match (hu, hd, hl, hr) {
            (false, false, false, false) => '┼',
            (true, true, true, true) => '╋',
            (true, true, false, false) => '╂',
            (false, false, true, true) => '┿',
            (true, false, false, false) => '╀',
            (false, true, false, false) => '╁',
            (false, false, true, false) => '┽',
            (false, false, false, true) => '┾',
            (true, false, true, false) => '╃',
            (true, false, false, true) => '╄',
            (false, true, true, false) => '╅',
            (false, true, false, true) => '╆',
            (true, false, true, true) => '╇',
            (false, true, true, true) => '╈',
            (true, true, true, false) => '╉',
            (true, true, false, true) => '╊',
        },

        // Single arms.
        (true, false, false, false) => {
            if hu {
                '╹'
            } else {
                '╵'
            }
        }
        (false, true, false, false) => {
            if hd {
                '╻'
            } else {
                '╷'
            }
        }
        (false, false, true, false) => {
            if hl {
                '╸'
            } else {
                '╴'
            }
        }
        (false, false, false, true) => {
            if hr {
                '╺'
            } else {
                '╶'
            }
        }

        (false, false, false, false) => ' ',
    }
}

// ── Hit testing ─────────────────────────────────────────────────────────

/// Which zone of a panel the coordinate falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelZone {
    /// Content interior (below title bar, left of scrollbar).
    Content,
    /// Top row (title bar).
    TitleBar,
    /// Right column (scrollbar track).
    Scrollbar,
}

/// Which panel contains the given screen coordinate?
///
/// A panel's chrome is its **top** row (title bar) and **right** column
/// (scrollbar). Points on chrome return `None`. Points in the content
/// interior return the panel index.
#[must_use]
pub fn panel_at(layout: &PanelLayout, x: u16, y: u16, show_borders: bool) -> Option<usize> {
    panel_zone_at(layout, x, y, show_borders).and_then(|(idx, zone)| {
        if zone == PanelZone::Content {
            Some(idx)
        } else {
            None
        }
    })
}

/// Hit-test a coordinate against the layout, returning the panel index and
/// which zone was hit.
///
/// A panel's owned chrome is its **top** row (title bar) and **right**
/// column (scrollbar). Points outside all panels return `None`.
#[must_use]
pub fn panel_zone_at(
    layout: &PanelLayout,
    x: u16,
    y: u16,
    show_borders: bool,
) -> Option<(usize, PanelZone)> {
    for panel in &layout.panels {
        let r = &panel.rect;
        // Must be within the panel's bounding rect.
        if x < r.x || x >= r.x + r.width || y < r.y || y >= r.y + r.height {
            continue;
        }
        let right = r.x + r.width.saturating_sub(1);
        if show_borders && y == r.y {
            return Some((panel.index, PanelZone::TitleBar));
        }
        if x == right {
            return Some((panel.index, PanelZone::Scrollbar));
        }
        return Some((panel.index, PanelZone::Content));
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn test_single_panel_fills_area() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![1]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 1);
        assert_eq!(layout.panels[0].rect, area);
    }

    #[test]
    fn test_two_panels_stacked() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![1, 1]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 2);
        // Both full width.
        assert_eq!(layout.panels[0].rect.width, 80);
        assert_eq!(layout.panels[1].rect.width, 80);
        // Each roughly half height, no overlap.
        assert_eq!(
            layout.panels[0].rect.height + layout.panels[1].rect.height,
            24
        );
        // Stacked vertically, no gap.
        assert_eq!(layout.panels[0].rect.y, 0);
        assert_eq!(
            layout.panels[1].rect.y, layout.panels[0].rect.height,
            "second panel starts where first ends"
        );
    }

    #[test]
    fn test_two_panels_side_by_side() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 2);
        // Both full height.
        assert_eq!(layout.panels[0].rect.height, 24);
        assert_eq!(layout.panels[1].rect.height, 24);
        // Each roughly half width, no overlap.
        assert_eq!(
            layout.panels[0].rect.width + layout.panels[1].rect.width,
            80
        );
        // Side by side, no gap.
        assert_eq!(layout.panels[0].rect.x, 0);
        assert_eq!(
            layout.panels[1].rect.x, layout.panels[0].rect.width,
            "second panel starts where first ends"
        );
    }

    #[test]
    fn test_three_panels_2_1() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2, 1]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 3);
        // Top row: two panels, widths sum to 80.
        assert_eq!(
            layout.panels[0].rect.width + layout.panels[1].rect.width,
            80
        );
        // Bottom row: one panel full width.
        assert_eq!(layout.panels[2].rect.width, 80);
        assert_eq!(layout.panels[2].rect.x, 0);
        // Heights sum to 24.
        assert_eq!(
            layout.panels[0].rect.height + layout.panels[2].rect.height,
            24
        );
        // Second row starts where first ends.
        assert_eq!(layout.panels[2].rect.y, layout.panels[0].rect.height);
    }

    #[test]
    fn test_pin_scaling() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let mut pinned = HashSet::new();
        pinned.insert(0);
        let layout = compute_layout(area, &comp, &pinned, 2.0);
        assert_eq!(layout.panels.len(), 2);
        // Panel 0 (pinned) should be wider than panel 1.
        assert!(
            layout.panels[0].rect.width > layout.panels[1].rect.width,
            "pinned panel 0 ({}) should be wider than unpinned panel 1 ({})",
            layout.panels[0].rect.width,
            layout.panels[1].rect.width,
        );
        // Widths still sum to area width.
        assert_eq!(
            layout.panels[0].rect.width + layout.panels[1].rect.width,
            80
        );
    }

    #[test]
    fn test_no_pixel_loss() {
        let area = Rect::new(0, 0, 81, 25);

        // Single-row: 3 panels side by side.
        let comp = Composition(vec![3]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        let total_width: u16 = layout.panels.iter().map(|p| p.rect.width).sum();
        assert_eq!(total_width, 81, "widths sum to area width");
        for p in &layout.panels {
            assert_eq!(p.rect.height, 25, "single-row panels are full height");
        }

        // Single-column: 3 panels stacked.
        let comp2 = Composition(vec![1, 1, 1]);
        let layout2 = compute_layout(area, &comp2, &HashSet::new(), 1.0);
        let total_height: u16 = layout2.panels.iter().map(|p| p.rect.height).sum();
        assert_eq!(total_height, 25, "heights sum to area height");
        for p in &layout2.panels {
            assert_eq!(p.rect.width, 81, "single-column panels are full width");
        }
    }

    #[test]
    fn test_curated_layouts_n1() {
        let layouts = curated_layouts(1);
        assert_eq!(layouts, vec![Composition(vec![1])]);
    }

    #[test]
    fn test_curated_layouts_n4() {
        let layouts = curated_layouts(4);
        assert!(
            layouts.contains(&Composition(vec![2, 2])),
            "N=4 curated layouts should contain (2,2)"
        );
    }

    #[test]
    fn test_closest_composition_add() {
        let old = Composition(vec![2, 1]);
        let result = closest_composition(&old, 4);
        assert_eq!(result.total(), 4);
        assert!(!result.0.is_empty());
    }

    #[test]
    fn test_closest_composition_remove() {
        let old = Composition(vec![2, 2]);
        let result = closest_composition(&old, 3);
        assert_eq!(result.total(), 3);
        assert_eq!(result.0, vec![2, 1]);
    }

    #[test]
    fn test_box_char_all_light() {
        // All-light corners.
        assert_eq!(box_char(ARM_DOWN | ARM_RIGHT), '┌');
        assert_eq!(box_char(ARM_DOWN | ARM_LEFT), '┐');
        assert_eq!(box_char(ARM_UP | ARM_RIGHT), '└');
        assert_eq!(box_char(ARM_UP | ARM_LEFT), '┘');
        // All-light T-junctions.
        assert_eq!(box_char(ARM_DOWN | ARM_LEFT | ARM_RIGHT), '┬');
        assert_eq!(box_char(ARM_UP | ARM_LEFT | ARM_RIGHT), '┴');
        assert_eq!(box_char(ARM_UP | ARM_DOWN | ARM_RIGHT), '├');
        assert_eq!(box_char(ARM_UP | ARM_DOWN | ARM_LEFT), '┤');
        // All-light cross.
        assert_eq!(box_char(ARM_UP | ARM_DOWN | ARM_LEFT | ARM_RIGHT), '┼');
    }

    #[test]
    fn test_box_char_focused_arms() {
        // Scrollbar (vertical, heavy) meets title bar (horizontal, light)
        // at a T-junction: heavy up+down, light right.
        let flags = ARM_UP | ARM_DOWN | ARM_RIGHT | HEAVY_UP | HEAVY_DOWN;
        assert_eq!(box_char(flags), '┠');

        // Title bar corner: heavy right (title of focused panel), light
        // down (scrollbar of unfocused panel below).
        let flags = ARM_DOWN | ARM_RIGHT | HEAVY_RIGHT;
        assert_eq!(box_char(flags), '┍');

        // Cross with focused panel owning top+right arms (heavy up, heavy left).
        let flags = ARM_UP | ARM_DOWN | ARM_LEFT | ARM_RIGHT | HEAVY_UP | HEAVY_LEFT;
        assert_eq!(box_char(flags), '╃');
    }

    #[test]
    fn test_panel_at_inside() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        // Point inside panel 0 content area.
        assert_eq!(panel_at(&layout, 10, 5, true), Some(0));
        // Point inside panel 1 content area.
        assert_eq!(panel_at(&layout, 50, 5, true), Some(1));
    }

    #[test]
    fn test_panel_at_border() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        let p0 = &layout.panels[0].rect;
        let p1 = &layout.panels[1].rect;
        // Top title row of panel 0 is chrome.
        assert_eq!(
            panel_at(&layout, 10, p0.y, true),
            None,
            "top title row is chrome"
        );
        // Right scrollbar column of panel 0 is chrome.
        let scrollbar_x = p0.x + p0.width - 1;
        assert_eq!(
            panel_at(&layout, scrollbar_x, 5, true),
            None,
            "right scrollbar is chrome"
        );
        // Left edge of panel 0 is content (no left border).
        assert_eq!(
            panel_at(&layout, p0.x, 1, true),
            Some(0),
            "left edge is content"
        );
        // Bottom edge of panel 0 is content (no bottom border).
        let bottom_y = p0.y + p0.height - 1;
        assert_eq!(
            panel_at(&layout, 10, bottom_y, true),
            Some(0),
            "bottom edge is content"
        );
        // Top title row of panel 1 is chrome.
        assert_eq!(
            panel_at(&layout, p1.x + 1, p1.y, true),
            None,
            "panel 1 title row"
        );
        // Right scrollbar of panel 1 is chrome.
        let scrollbar_x1 = p1.x + p1.width - 1;
        assert_eq!(
            panel_at(&layout, scrollbar_x1, 5, true),
            None,
            "panel 1 scrollbar"
        );
    }
}
