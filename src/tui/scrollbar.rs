// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Sub-character scrollbar with fractional block characters (1/8 precision)
//! and overflow count indicators (`N▲` / `N▼`).
//!
//! The right border of each panel doubles as the scrollbar track. The thumb
//! size is proportional to the viewport/buffer ratio. When content fits in the
//! viewport, no scrollbar is rendered.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

// ── Data types ──────────────────────────────────────────────────────────

/// Input state for scrollbar computation.
#[derive(Debug, Clone)]
pub struct ScrollMetrics {
    /// Total number of content lines.
    pub content_length: usize,
    /// Number of visible lines in the viewport.
    pub viewport_length: usize,
    /// Current scroll position (index of first visible line).
    pub position: usize,
}

/// Computed thumb position with sub-character precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbPosition {
    /// First cell of the thumb (0-indexed from top of track).
    pub start_cell: u16,
    /// Eighth offset within `start_cell` (0–7, 0 = full cell from top).
    pub start_eighth: u8,
    /// Last cell of the thumb (inclusive).
    pub end_cell: u16,
    /// Eighth offset within `end_cell` (0–7, 7 = full cell).
    pub end_eighth: u8,
}

/// Overflow counts for a panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowCounts {
    /// Lines above the viewport.
    pub above: usize,
    /// Lines below the viewport.
    pub below: usize,
}

// ── Computation ─────────────────────────────────────────────────────────

/// Compute the thumb position with sub-character precision.
///
/// Returns `None` if content fits in the viewport (no scrollbar needed).
///
/// The thumb size is proportional to `viewport / content`, clamped to a
/// minimum of 8 eighths (1 full cell). The position maps the scroll offset
/// to the available track space.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "track coordinates are always small"
)]
pub fn compute_thumb(metrics: &ScrollMetrics, track_height: u16) -> Option<ThumbPosition> {
    if metrics.content_length <= metrics.viewport_length || track_height == 0 {
        return None;
    }

    let track_eighths = u64::from(track_height) * 8;

    // Thumb size in eighths, proportional to viewport/content ratio.
    let thumb_eighths = ((metrics.viewport_length as u64) * track_eighths
        / (metrics.content_length as u64))
        .max(8) // minimum 1 full cell
        .min(track_eighths);

    // Scrollable range: content_length - viewport_length positions map to
    // track_eighths - thumb_eighths positions.
    let scrollable = metrics.content_length - metrics.viewport_length;
    let available = track_eighths - thumb_eighths;

    let start_eighth_abs = if scrollable == 0 {
        0
    } else {
        (metrics.position as u64) * available / (scrollable as u64)
    };

    let end_eighth_abs = start_eighth_abs + thumb_eighths - 1;

    Some(ThumbPosition {
        start_cell: (start_eighth_abs / 8) as u16,
        start_eighth: (start_eighth_abs % 8) as u8,
        end_cell: (end_eighth_abs / 8) as u16,
        end_eighth: (end_eighth_abs % 8) as u8,
    })
}

/// Map 0–8 eighths to a lower fractional block character.
///
/// | Eighths | Char |
/// |---------|------|
/// | 0       | ' '  |
/// | 1       | '▁'  |
/// | 2       | '▂'  |
/// | 3       | '▃'  |
/// | 4       | '▄'  |
/// | 5       | '▅'  |
/// | 6       | '▆'  |
/// | 7       | '▇'  |
/// | 8       | '█'  |
#[must_use]
pub const fn fractional_block_lower(eighths: u8) -> char {
    match eighths {
        1 => '▁',
        2 => '▂',
        3 => '▃',
        4 => '▄',
        5 => '▅',
        6 => '▆',
        7 => '▇',
        8 => '█',
        _ => ' ',
    }
}

/// Map 1–8 eighths to an "upper" fractional block.
///
/// Unicode lacks dedicated upper-block characters, so we use fg/bg color
/// swapping: render a lower block of `(8 - eighths)` with swapped colors.
///
/// Returns `(character, swap_colors)` where `swap_colors` indicates that
/// fg and bg should be exchanged when rendering.
#[must_use]
pub const fn fractional_block_upper(eighths: u8) -> (char, bool) {
    if eighths >= 8 {
        ('█', false)
    } else if eighths == 0 {
        (' ', false)
    } else {
        (fractional_block_lower(8 - eighths), true)
    }
}

/// Compute overflow counts for a panel.
///
/// - `above` = lines above the viewport (i.e., the scroll position).
/// - `below` = lines below the viewport.
#[must_use]
pub const fn compute_overflow(metrics: &ScrollMetrics) -> OverflowCounts {
    OverflowCounts {
        above: metrics.position,
        below: metrics
            .content_length
            .saturating_sub(metrics.position + metrics.viewport_length),
    }
}

/// Given a click on the scrollbar track, compute the scroll position that
/// centers the thumb on the clicked point.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "track coordinates are always small"
)]
pub fn scroll_position_from_click(y: u16, track_area: Rect, metrics: &ScrollMetrics) -> usize {
    if metrics.content_length <= metrics.viewport_length {
        return 0;
    }

    let track_height = track_area.height;
    if track_height == 0 {
        return 0;
    }

    // Offset within the track (clamped to valid range).
    let click_offset = y
        .saturating_sub(track_area.y)
        .min(track_height.saturating_sub(1));

    let scrollable = metrics.content_length - metrics.viewport_length;

    // Map click position to scroll position.
    let position = u64::from(click_offset) * (scrollable as u64)
        / u64::from(track_height.saturating_sub(1).max(1));

    (position as usize).min(scrollable)
}

// ── Rendering ───────────────────────────────────────────────────────────

/// Render the scrollbar into the panel's right border column.
///
/// The right column IS the scrollbar — it is the panel's owned right border,
/// not a separate element inside a border frame. Each cell gets a fractional
/// block character with appropriate fg/bg coloring.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_scrollbar(
    metrics: &ScrollMetrics,
    track_area: Rect,
    buf: &mut Buffer,
    thumb_color: Color,
    track_color: Color,
) {
    let Some(thumb) = compute_thumb(metrics, track_area.height) else {
        return;
    };

    for row in 0..track_area.height {
        let x = track_area.x;
        let y = track_area.y + row;

        if y >= buf.area.y + buf.area.height || x >= buf.area.x + buf.area.width {
            continue;
        }

        let cell = &mut buf[(x, y)];

        if row < thumb.start_cell || row > thumb.end_cell {
            // Outside thumb — empty track.
            cell.set_char(' ');
            cell.set_style(Style::default().bg(track_color));
        } else if row == thumb.start_cell && row == thumb.end_cell {
            // Thumb fits in a single cell — handle both start and end fractions.
            let top_skip = thumb.start_eighth;
            let bottom_fill = thumb.end_eighth + 1;
            let fill = bottom_fill.saturating_sub(top_skip);

            if top_skip == 0 {
                // Starts at top of cell — use lower block.
                cell.set_char(fractional_block_lower(fill));
                cell.set_style(Style::default().fg(thumb_color).bg(track_color));
            } else {
                // Starts partway down — use upper-style rendering with fg/bg swap.
                let (ch, swap) = fractional_block_upper(fill);
                cell.set_char(ch);
                cell.set_style(if swap {
                    Style::default().fg(track_color).bg(thumb_color)
                } else {
                    Style::default().fg(thumb_color).bg(track_color)
                });
            }
        } else if row == thumb.start_cell {
            // Top partial cell of the thumb.
            // start_eighth = how many eighths from the top are empty.
            // Fill = 8 - start_eighth eighths from the bottom.
            let fill = 8 - thumb.start_eighth;
            cell.set_char(fractional_block_lower(fill));
            cell.set_style(Style::default().fg(thumb_color).bg(track_color));
        } else if row == thumb.end_cell {
            // Bottom partial cell of the thumb.
            // end_eighth = how many eighths from the top are filled (0-indexed).
            let fill = thumb.end_eighth + 1;
            let (ch, swap) = fractional_block_upper(fill);
            cell.set_char(ch);
            cell.set_style(if swap {
                Style::default().fg(track_color).bg(thumb_color)
            } else {
                Style::default().fg(thumb_color).bg(track_color)
            });
        } else {
            // Fully inside thumb.
            cell.set_char('█');
            cell.set_style(Style::default().fg(thumb_color).bg(track_color));
        }
    }
}

/// Render overflow count indicators into the content area.
///
/// - If `above > 0`: render ` {above}▲` right-aligned in the first row.
/// - If `below > 0`: render ` {below}▼` right-aligned in the last row.
///
/// A leading space separates the count from event text. Counts overlay
/// event content (right-aligned).
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_overflow_counts(
    counts: &OverflowCounts,
    content_area: Rect,
    buf: &mut Buffer,
    style: Style,
) {
    if content_area.width == 0 || content_area.height == 0 {
        return;
    }

    if counts.above > 0 {
        let text = format!(" {}▲", counts.above);
        let text_width = UnicodeWidthStr::width(text.as_str()) as u16;
        if text_width <= content_area.width {
            let x = content_area.x + content_area.width - text_width;
            let y = content_area.y;
            let line = Line::from(Span::styled(text, style));
            buf.set_line(x, y, &line, text_width);
        }
    }

    if counts.below > 0 {
        let text = format!(" {}▼", counts.below);
        let text_width = UnicodeWidthStr::width(text.as_str()) as u16;
        if text_width <= content_area.width {
            let x = content_area.x + content_area.width - text_width;
            let y = content_area.y + content_area.height - 1;
            let line = Line::from(Span::styled(text, style));
            buf.set_line(x, y, &line, text_width);
        }
    }
}

/// Which overflow indicator was clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowHit {
    /// Clicked the top indicator — jump to top.
    Top,
    /// Clicked the bottom indicator — jump to bottom.
    Bottom,
}

/// Hit-test a click against the overflow count indicators.
///
/// The hit zone covers the digits and arrow (`15▲`) but excludes the
/// leading padding space. Returns `None` if the click didn't land on
/// either indicator.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn overflow_hit_test(
    x: u16,
    y: u16,
    content_area: Rect,
    counts: &OverflowCounts,
) -> Option<OverflowHit> {
    if content_area.width == 0 || content_area.height == 0 {
        return None;
    }

    let right = content_area.x + content_area.width;

    if counts.above > 0 && y == content_area.y {
        // Hit zone: digits + arrow, excluding the leading space.
        let label = format!("{}▲", counts.above);
        let label_width = UnicodeWidthStr::width(label.as_str()) as u16;
        if label_width <= content_area.width && x >= right - label_width && x < right {
            return Some(OverflowHit::Top);
        }
    }

    let bottom_y = content_area.y + content_area.height - 1;
    if counts.below > 0 && y == bottom_y {
        let label = format!("{}▼", counts.below);
        let label_width = UnicodeWidthStr::width(label.as_str()) as u16;
        if label_width <= content_area.width && x >= right - label_width && x < right {
            return Some(OverflowHit::Bottom);
        }
    }

    None
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_no_scrollbar_when_fits() {
        let metrics = ScrollMetrics {
            content_length: 10,
            viewport_length: 20,
            position: 0,
        };
        assert!(
            compute_thumb(&metrics, 20).is_none(),
            "should return None when content fits in viewport"
        );
    }

    #[test]
    fn test_thumb_proportional_size() {
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 20,
            position: 0,
        };
        let thumb = compute_thumb(&metrics, 20).expect("should produce a thumb");
        // Expected: 20 * 8 * 20 / 100 = 32 eighths = 4 cells.
        let start_abs = u64::from(thumb.start_cell) * 8 + u64::from(thumb.start_eighth);
        let end_abs = u64::from(thumb.end_cell) * 8 + u64::from(thumb.end_eighth);
        let size = end_abs - start_abs + 1;
        assert_eq!(size, 32, "thumb should be 32 eighths (4 cells)");
    }

    #[test]
    fn test_thumb_minimum_size() {
        let metrics = ScrollMetrics {
            content_length: 10000,
            viewport_length: 1,
            position: 0,
        };
        let thumb = compute_thumb(&metrics, 20).expect("should produce a thumb");
        let start_abs = u64::from(thumb.start_cell) * 8 + u64::from(thumb.start_eighth);
        let end_abs = u64::from(thumb.end_cell) * 8 + u64::from(thumb.end_eighth);
        let size = end_abs - start_abs + 1;
        assert_eq!(size, 8, "thumb should be clamped to minimum 8 eighths");
    }

    #[test]
    fn test_thumb_position_top() {
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 20,
            position: 0,
        };
        let thumb = compute_thumb(&metrics, 20).expect("should produce a thumb");
        assert_eq!(thumb.start_cell, 0, "thumb should start at cell 0");
        assert_eq!(thumb.start_eighth, 0, "thumb should start at eighth 0");
    }

    #[test]
    fn test_thumb_position_bottom() {
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 20,
            position: 80, // max = content - viewport
        };
        let thumb = compute_thumb(&metrics, 20).expect("should produce a thumb");
        assert_eq!(
            thumb.end_cell, 19,
            "thumb should end at last cell (track_height - 1)"
        );
        assert_eq!(thumb.end_eighth, 7, "thumb should end at eighth 7");
    }

    #[test]
    fn test_thumb_position_middle() {
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 20,
            position: 40, // midpoint
        };
        let thumb = compute_thumb(&metrics, 20).expect("should produce a thumb");
        // Track is 160 eighths, thumb is 32 eighths.
        // Position 40/80 = 0.5 → start at eighth 64 of 128 available = 64.
        // Thumb center should be roughly at 80 (middle of 160).
        let start_abs = u64::from(thumb.start_cell) * 8 + u64::from(thumb.start_eighth);
        let end_abs = u64::from(thumb.end_cell) * 8 + u64::from(thumb.end_eighth);
        let center = u64::midpoint(start_abs, end_abs);
        // Center should be roughly 80 (middle of track).
        assert!(
            (70..=90).contains(&center),
            "thumb center {center} should be roughly in the middle of the track"
        );
    }

    #[test]
    fn test_subchar_precision_160_positions() {
        let content = 1000;
        let viewport = 20;
        let track_height = 20u16;
        let max_pos = content - viewport;

        let mut prev_abs = 0u64;
        let mut positions = Vec::new();

        for pos in 0..=max_pos {
            let metrics = ScrollMetrics {
                content_length: content,
                viewport_length: viewport,
                position: pos,
            };
            let thumb = compute_thumb(&metrics, track_height).expect("should produce a thumb");
            let abs = u64::from(thumb.start_cell) * 8 + u64::from(thumb.start_eighth);
            positions.push(abs);

            if pos > 0 {
                assert!(
                    abs >= prev_abs,
                    "position should be monotonically non-decreasing: pos={pos}, abs={abs}, prev={prev_abs}"
                );
            }
            prev_abs = abs;
        }

        // Should cover the full range: first position starts at 0, last covers
        // the end of the track.
        assert_eq!(positions[0], 0, "first position should start at 0");
        let last = *positions.last().expect("non-empty");
        // Last thumb starts at track_eighths - thumb_eighths.
        let track_eighths = u64::from(track_height) * 8;
        let thumb_eighths = ((viewport as u64) * track_eighths / (content as u64)).max(8);
        let expected_last = track_eighths - thumb_eighths;
        assert_eq!(
            last, expected_last,
            "last position should cover end of track"
        );
    }

    #[test]
    fn test_fractional_block_lower_all_arms() {
        assert_eq!(fractional_block_lower(0), ' ');
        assert_eq!(fractional_block_lower(1), '▁');
        assert_eq!(fractional_block_lower(2), '▂');
        assert_eq!(fractional_block_lower(3), '▃');
        assert_eq!(fractional_block_lower(4), '▄');
        assert_eq!(fractional_block_lower(5), '▅');
        assert_eq!(fractional_block_lower(6), '▆');
        assert_eq!(fractional_block_lower(7), '▇');
        assert_eq!(fractional_block_lower(8), '█');
        // Out of range → space.
        assert_eq!(fractional_block_lower(9), ' ');
        assert_eq!(fractional_block_lower(255), ' ');
    }

    #[test]
    fn test_fractional_block_upper() {
        // Full block: eighths >= 8 → ('█', false).
        assert_eq!(fractional_block_upper(8), ('█', false));
        assert_eq!(fractional_block_upper(9), ('█', false));
        // Empty: eighths == 0 → (' ', false).
        assert_eq!(fractional_block_upper(0), (' ', false));
        // Partial: uses lower(8-n) with color swap.
        assert_eq!(fractional_block_upper(1), ('▇', true)); // lower(7)
        assert_eq!(fractional_block_upper(2), ('▆', true)); // lower(6)
        assert_eq!(fractional_block_upper(4), ('▄', true)); // lower(4)
        assert_eq!(fractional_block_upper(7), ('▁', true)); // lower(1)
    }

    #[test]
    fn test_overflow_counts_none() {
        let metrics = ScrollMetrics {
            content_length: 10,
            viewport_length: 20,
            position: 0,
        };
        let counts = compute_overflow(&metrics);
        assert_eq!(counts.above, 0);
        assert_eq!(counts.below, 0);
    }

    #[test]
    fn test_overflow_counts_scrolled_middle() {
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 20,
            position: 40,
        };
        let counts = compute_overflow(&metrics);
        assert_eq!(counts.above, 40);
        assert_eq!(counts.below, 40);
    }

    #[test]
    fn test_overflow_counts_at_bottom() {
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 20,
            position: 80,
        };
        let counts = compute_overflow(&metrics);
        assert_eq!(counts.above, 80);
        assert_eq!(counts.below, 0);
    }

    #[test]
    fn test_render_scrollbar_basic() {
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 10,
            position: 0,
        };
        let track_area = Rect::new(0, 0, 1, 10);

        let backend = TestBackend::new(1, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                render_scrollbar(
                    &metrics,
                    track_area,
                    f.buffer_mut(),
                    Color::White,
                    Color::DarkGray,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // Thumb is 10% of track → 1 cell at position 0.
        // First cell should be the thumb (█).
        assert_eq!(buf[(0, 0)].symbol(), "█", "first cell should be thumb");
        // Cells below the thumb should be track (space).
        // The thumb is at least 1 cell, so check a cell well below.
        assert_eq!(
            buf[(0, 5)].symbol(),
            " ",
            "cell below thumb should be empty track"
        );
    }

    #[test]
    fn test_render_overflow_counts() {
        let counts = OverflowCounts {
            above: 15,
            below: 30,
        };
        let content_area = Rect::new(0, 0, 40, 10);
        let style = Style::default().fg(Color::DarkGray);

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                render_overflow_counts(&counts, content_area, f.buffer_mut(), style);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // Check top-right for " 15▲" (space + number + arrow).
        let mut top_right = String::new();
        for x in 36..40 {
            top_right.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            top_right.contains("15▲"),
            "expected 15▲ in top-right, got: {top_right:?}"
        );

        // Check bottom-right for " 30▼".
        let mut bottom_right = String::new();
        for x in 36..40 {
            bottom_right.push_str(buf[(x, 9)].symbol());
        }
        assert!(
            bottom_right.contains("30▼"),
            "expected 30▼ in bottom-right, got: {bottom_right:?}"
        );
    }

    #[test]
    fn test_scroll_position_from_click() {
        let track_area = Rect::new(0, 0, 1, 20);
        let metrics = ScrollMetrics {
            content_length: 200,
            viewport_length: 20,
            position: 0,
        };

        // Click at the middle of the track (cell 10 of 20).
        let pos = scroll_position_from_click(10, track_area, &metrics);
        // scrollable = 180, click at 10/19 ≈ 0.526 → ~94.7.
        // Allow some tolerance for integer rounding.
        assert!(
            (85..=100).contains(&pos),
            "expected position roughly 90, got {pos}"
        );

        // Click at top should give position 0.
        let pos_top = scroll_position_from_click(0, track_area, &metrics);
        assert_eq!(pos_top, 0, "click at top should give position 0");

        // Click at bottom should give max position.
        let pos_bottom = scroll_position_from_click(19, track_area, &metrics);
        assert_eq!(pos_bottom, 180, "click at bottom should give max position");
    }

    #[test]
    fn test_overflow_hit_test_top() {
        let content_area = Rect::new(0, 0, 40, 10);
        let counts = OverflowCounts {
            above: 15,
            below: 5,
        };

        // "15▲" is 3 columns wide, right-aligned at columns 37..40.
        // Click on the digits (x=37) → Top.
        assert_eq!(
            overflow_hit_test(37, 0, content_area, &counts),
            Some(OverflowHit::Top)
        );
        // Click on the arrow (x=39).
        assert_eq!(
            overflow_hit_test(39, 0, content_area, &counts),
            Some(OverflowHit::Top)
        );
        // Click on the leading space (x=36) → miss.
        assert_eq!(overflow_hit_test(36, 0, content_area, &counts), None);
        // Click on the right row but wrong y → miss.
        assert_eq!(overflow_hit_test(38, 1, content_area, &counts), None);
    }

    #[test]
    fn test_overflow_hit_test_bottom() {
        let content_area = Rect::new(0, 0, 40, 10);
        let counts = OverflowCounts { above: 0, below: 5 };

        // "5▼" is 2 columns wide, right-aligned at columns 38..40.
        assert_eq!(
            overflow_hit_test(38, 9, content_area, &counts),
            Some(OverflowHit::Bottom)
        );
        assert_eq!(
            overflow_hit_test(39, 9, content_area, &counts),
            Some(OverflowHit::Bottom)
        );
        // Leading space at x=37 → miss.
        assert_eq!(overflow_hit_test(37, 9, content_area, &counts), None);
    }

    #[test]
    fn test_render_scrollbar_multi_cell_thumb() {
        // 50% viewport → thumb spans ~5 cells. Position at midpoint.
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 50,
            position: 25, // 50% through scrollable range
        };
        let track_area = Rect::new(0, 0, 1, 10);

        let backend = TestBackend::new(1, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                render_scrollbar(
                    &metrics,
                    track_area,
                    f.buffer_mut(),
                    Color::White,
                    Color::DarkGray,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let thumb = compute_thumb(&metrics, 10).expect("should have thumb");

        // Cells before thumb should be empty track.
        if thumb.start_cell > 0 {
            assert_eq!(
                buf[(0, 0)].symbol(),
                " ",
                "cell before thumb should be empty track"
            );
        }

        // Middle cells of the thumb should be full blocks.
        let mid = u16::midpoint(thumb.start_cell, thumb.end_cell);
        if mid > thumb.start_cell && mid < thumb.end_cell {
            assert_eq!(
                buf[(0, mid)].symbol(),
                "█",
                "middle cell of multi-cell thumb should be full block"
            );
        }

        // Cells after thumb should be empty track.
        if thumb.end_cell < 9 {
            assert_eq!(
                buf[(0, 9)].symbol(),
                " ",
                "cell after thumb should be empty track"
            );
        }
    }

    #[test]
    fn test_render_scrollbar_thumb_not_at_zero() {
        // Thumb at the bottom — verifies row < start_cell renders empty track.
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 10,
            position: 80,
        };
        let track_area = Rect::new(0, 0, 1, 10);

        let backend = TestBackend::new(1, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                render_scrollbar(
                    &metrics,
                    track_area,
                    f.buffer_mut(),
                    Color::White,
                    Color::DarkGray,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let thumb = compute_thumb(&metrics, 10).expect("should have thumb");

        // Thumb should be near the bottom (start_cell > 0).
        assert!(
            thumb.start_cell > 0,
            "thumb should not start at cell 0: {thumb:?}"
        );

        // Top of track should be empty (before thumb).
        assert_eq!(
            buf[(0, 0)].symbol(),
            " ",
            "top should be empty track when thumb is near bottom"
        );

        // The thumb cell itself should not be empty.
        assert_ne!(
            buf[(0, thumb.start_cell)].symbol(),
            " ",
            "thumb cell should have content"
        );
    }

    #[test]
    fn test_render_scrollbar_per_cell_characters() {
        // Engineered to produce fractional edges on distinct cells.
        // content=100, viewport=20, position=11, track=20:
        //   thumb: start_cell=2 start_eighth=1, end_cell=6 end_eighth=0
        //   cell 0-1: track (' ')
        //   cell 2: top partial, fill=7 → '▇' (lower block)
        //   cell 3-5: fully inside → '█'
        //   cell 6: bottom partial, fill=1 → fractional_block_upper(1) = '▇' (swapped)
        //   cell 7-19: track (' ')
        let metrics = ScrollMetrics {
            content_length: 100,
            viewport_length: 20,
            position: 11,
        };
        let track_area = Rect::new(0, 0, 1, 20);

        let backend = TestBackend::new(1, 20);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                render_scrollbar(
                    &metrics,
                    track_area,
                    f.buffer_mut(),
                    Color::White,
                    Color::DarkGray,
                );
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let thumb = compute_thumb(&metrics, 20).expect("should have thumb");

        // Verify the thumb has fractional edges on separate cells.
        assert_ne!(
            thumb.start_cell, thumb.end_cell,
            "need multi-cell thumb: {thumb:?}"
        );
        assert!(thumb.start_eighth > 0, "need fractional start: {thumb:?}");

        // Track before thumb.
        assert_eq!(buf[(0, 0)].symbol(), " ", "cell 0: track");
        assert_eq!(buf[(0, 1)].symbol(), " ", "cell 1: track");

        // Top partial cell: fill = 8 - start_eighth = 7 → '▇'.
        assert_eq!(
            buf[(0, thumb.start_cell)].symbol(),
            "▇",
            "start cell: top partial 7/8"
        );

        // Interior cells: full block.
        for row in (thumb.start_cell + 1)..thumb.end_cell {
            assert_eq!(
                buf[(0, row)].symbol(),
                "█",
                "cell {row}: fully inside thumb"
            );
        }

        // Bottom partial cell: fill = end_eighth + 1 → fractional_block_upper.
        let bottom_fill = thumb.end_eighth + 1;
        let (expected_char, _) = fractional_block_upper(bottom_fill);
        assert_eq!(
            buf[(0, thumb.end_cell)].symbol(),
            expected_char.to_string(),
            "end cell: bottom partial"
        );

        // Track after thumb.
        assert_eq!(
            buf[(0, thumb.end_cell + 1)].symbol(),
            " ",
            "cell after thumb: track"
        );
    }

    #[test]
    fn test_render_overflow_counts_zero_dimensions() {
        let counts = OverflowCounts {
            above: 10,
            below: 10,
        };
        let style = Style::default();

        // Zero width — should not panic or render.
        let backend = TestBackend::new(1, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 0, 10);
                render_overflow_counts(&counts, area, f.buffer_mut(), style);
            })
            .expect("draw");

        // Zero height — should not panic or render.
        let backend = TestBackend::new(40, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 0);
                render_overflow_counts(&counts, area, f.buffer_mut(), style);
            })
            .expect("draw");
    }

    #[test]
    fn test_overflow_hit_test_at_right_boundary() {
        let content_area = Rect::new(0, 0, 40, 10);
        let counts = OverflowCounts { above: 5, below: 5 };
        let right = content_area.x + content_area.width; // = 40

        // x == right should be a miss (x < right, not <=).
        assert_eq!(
            overflow_hit_test(right, 0, content_area, &counts),
            None,
            "x at right boundary should miss"
        );
    }

    #[test]
    fn test_overflow_hit_test_zero_dimensions() {
        let counts = OverflowCounts { above: 5, below: 5 };

        // Zero width.
        assert_eq!(
            overflow_hit_test(0, 0, Rect::new(0, 0, 0, 10), &counts),
            None,
        );
        // Zero height.
        assert_eq!(
            overflow_hit_test(0, 0, Rect::new(0, 0, 40, 0), &counts),
            None,
        );
    }

    #[test]
    fn test_overflow_hit_test_no_overflow() {
        let content_area = Rect::new(0, 0, 40, 10);
        let counts = OverflowCounts { above: 0, below: 0 };

        assert_eq!(overflow_hit_test(39, 0, content_area, &counts), None);
        assert_eq!(overflow_hit_test(39, 9, content_area, &counts), None);
    }
}
