// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The interactive health/config dashboard — a 2×2 master-detail grid over the
//! daemon's `state.json` snapshot and the health model's findings.
//!
//! Four panes share a grid: the **root/server tree** (top-left), the
//! **client/session tree** (bottom-left), the **contextual detail pane**
//! (top-right), and the **problems pane** (bottom-right, the durable
//! notification surface). A header strip carries the verdict, daemon identity,
//! version/skew, and snapshot staleness; a footer carries the keybinding hint.
//!
//! It is a **pure file reader**: it file-watches the snapshot and re-loads on
//! change, reads config files for findings, and **never** opens the firehose or
//! probes an LSP (structurally unwedgeable stays a feature — DESIGN). The bridge
//! to `catenary query` is a yankable scope id (OSC 52).

pub mod action;
pub mod app;
pub mod data;
pub mod findings;
pub mod format;
pub mod hints;
pub mod icons;
pub mod model;
pub mod render;
pub mod scrollbar;
pub mod theme;

pub use app::App;
pub use data::DataSource;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::Watcher;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use crate::config::IconConfig;

use self::app::Cursor;
use self::data::StateJsonDataSource;
use self::hints::{KEYBINDS_EXPANDED_HEIGHT, render_keybinds_content};
use self::icons::IconSet;
use self::model::Pane;
use self::scrollbar::{OverflowCounts, render_overflow_counts};
use self::theme::Theme;

/// Tick interval: the snapshot is re-read each tick (a small file read).
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Terminal width below which the grid degrades to stacked full-width panes.
const NARROW_THRESHOLD: u16 = 80;

/// Left-column width as a percentage of the grid (the trees).
const LEFT_PCT: u16 = 46;

/// Start a file watcher on the snapshot's parent directory (atomic-rename safe).
fn start_state_watcher(
    snapshot_path: &Path,
) -> Result<(notify::RecommendedWatcher, mpsc::Receiver<()>)> {
    let file_name = snapshot_path.file_name().unwrap_or_default().to_os_string();
    let (tx, rx) = mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let matches = event
                .paths
                .iter()
                .any(|p| p.file_name() == Some(&file_name));
            if matches {
                let _ = tx.send(());
            }
        }
    })?;

    let watch_dir = snapshot_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if !watch_dir.exists() {
        let _ = std::fs::create_dir_all(&watch_dir);
    }
    watcher.watch(&watch_dir, notify::RecursiveMode::NonRecursive)?;

    Ok((watcher, rx))
}

/// Run the interactive dashboard against the live `state.json`.
///
/// # Errors
///
/// Returns an error if terminal setup fails or the initial snapshot read fails.
pub fn run(icon_config: IconConfig) -> Result<()> {
    let data = StateJsonDataSource::new();
    let snapshot_path = data.path().to_path_buf();
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let watcher = match start_state_watcher(&snapshot_path) {
        Ok((watcher, rx)) => Some((watcher, rx)),
        Err(e) => {
            tracing::info!("state.json watcher unavailable, polling instead: {e}");
            None
        }
    };
    let (_watcher, watch_rx) = match watcher {
        Some((w, rx)) => (Some(w), Some(rx)),
        None => (None, None),
    };

    run_with_data_and_watcher(icon_config, project_root, Box::new(data), watch_rx.as_ref())
}

/// Run the dashboard with an explicit data source and optional change signal.
fn run_with_data_and_watcher(
    icon_config: IconConfig,
    project_root: PathBuf,
    data: Box<dyn DataSource>,
    watch_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let theme = Theme::new();
    let icons = IconSet::from_config(icon_config);

    let mut app = App::new(&theme, &icons, project_root, data)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app, watch_rx);

    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Stored pane rectangles for mouse dispatch (inner content areas).
#[derive(Default)]
struct PanelRects {
    root: Rect,
    session: Rect,
    detail: Rect,
    problems: Rect,
}

/// Handle a key event.
fn handle_key(app: &mut App<'_>, code: KeyCode) {
    // A guided-install consent overlay is modal: Enter runs, Esc dismisses.
    if app.pending_install.is_some() {
        handle_install_key(app, code);
        return;
    }
    // A guided-mutation consent overlay is modal: it captures every key until
    // the user confirms or declines. Value edits (a binary path) take chars.
    if app.pending_action.is_some() {
        handle_action_key(app, code);
        return;
    }
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') => app.toggle_keybinds(),
        KeyCode::Char('a') => app.begin_action(),
        KeyCode::Tab => app.cycle_focus(),
        KeyCode::BackTab => app.cycle_focus_back(),
        KeyCode::Char('j') | KeyCode::Down => app.cursor_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.cursor_up(1),
        KeyCode::PageDown => app.page_down(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::Char('g') | KeyCode::Home => app.jump_home(),
        KeyCode::Char('G') | KeyCode::End => app.jump_end(),
        KeyCode::Enter => app.activate(),
        KeyCode::Char('p') => app.toggle_problems_only(),
        KeyCode::Char('d') => {
            // 'd' expands the dormant tail when the root tree is focused.
            app.set_focus(Pane::RootTree);
            app.jump_end();
            app.activate();
        }
        KeyCode::Char('y') => {
            if let Some(text) = app.selected_yank_text() {
                osc52_copy(&text);
            }
        }
        _ => {}
    }
}

/// Handle a key while the guided-mutation consent overlay is open.
fn handle_action_key(app: &mut App<'_>, code: KeyCode) {
    match code {
        KeyCode::Enter => app.confirm_action(),
        KeyCode::Esc => app.cancel_action(),
        KeyCode::Tab | KeyCode::BackTab => app.action_cycle_layer(),
        KeyCode::Backspace => app.action_backspace(),
        KeyCode::Char(c) => app.action_push_char(c),
        _ => {}
    }
}

/// Handle a key while the guided-install consent overlay is open. Enter runs the
/// pinned, verified install (a no-op once it has run); the escape key dismisses.
fn handle_install_key(app: &mut App<'_>, code: KeyCode) {
    match code {
        KeyCode::Enter => app.confirm_install(),
        KeyCode::Esc => app.cancel_install(),
        _ => {}
    }
}

/// Copy text to the system clipboard via OSC 52.
fn osc52_copy(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::style::Print(format!("\x1b]52;c;{encoded}\x07"))
    );
}

/// Minimal base64 encoder (RFC 4648).
#[allow(
    clippy::cast_possible_truncation,
    reason = "base64 index is always 0..63; byte-to-char is ASCII"
)]
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Handle a mouse event: click focuses a pane and moves its cursor; scroll
/// moves the cursor of the pane under the pointer.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
fn handle_mouse(
    app: &mut App<'_>,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    rects: &PanelRects,
) {
    // The consent overlays are keyboard-only; ignore clicks behind them.
    if app.pending_action.is_some() || app.pending_install.is_some() {
        return;
    }
    let pos = Rect {
        x: column,
        y: row,
        width: 1,
        height: 1,
    };
    let target = if rects.root.intersects(pos) {
        Some(Pane::RootTree)
    } else if rects.session.intersects(pos) {
        Some(Pane::SessionTree)
    } else if rects.detail.intersects(pos) {
        Some(Pane::Detail)
    } else if rects.problems.intersects(pos) {
        Some(Pane::Problems)
    } else {
        None
    };
    let Some(pane) = target else { return };

    match kind {
        MouseEventKind::ScrollUp => {
            app.set_focus(pane);
            app.cursor_up(3);
        }
        MouseEventKind::ScrollDown => {
            app.set_focus(pane);
            app.cursor_down(3);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.set_focus(pane);
            // Move the cursor to the clicked row, then activate (expand/focus).
            move_cursor_to_click(app, pane, row, rects);
            app.activate();
        }
        _ => {}
    }
}

/// Move the focused pane's cursor to the row under a click (best-effort, single
/// line per entry for the trees).
fn move_cursor_to_click(app: &mut App<'_>, pane: Pane, row: u16, rects: &PanelRects) {
    let (inner, cursor, len) = match pane {
        Pane::RootTree => (rects.root, &mut app.root_cursor, app.root_rows.len()),
        Pane::SessionTree => (
            rects.session,
            &mut app.session_cursor,
            app.session_rows.len(),
        ),
        Pane::Problems => (
            rects.problems,
            &mut app.problem_cursor,
            app.problem_rows.len(),
        ),
        Pane::Detail => return,
    };
    // Body starts one row below the title.
    if row <= inner.y {
        return;
    }
    let rel = (row - inner.y - 1) as usize;
    let target = cursor.scroll + rel;
    if target < len {
        cursor.index = target;
        cursor.settle(len);
    }
}

/// Draw the shared grid borders (outer box + interior cross) and return the four
/// inner content rects. Titles render inside each body, not on the border.
fn draw_grid_borders(
    area: Rect,
    split_x: u16,
    split_y: u16,
    buf: &mut Buffer,
    style: Style,
) -> [Rect; 4] {
    let x0 = area.x;
    let y0 = area.y;
    let x1 = area.x + area.width - 1;
    let y1 = area.y + area.height - 1;

    // Edges.
    for x in x0..=x1 {
        set(buf, x, y0, "─", style);
        set(buf, x, y1, "─", style);
    }
    for y in y0..=y1 {
        set(buf, x0, y, "│", style);
        set(buf, x1, y, "│", style);
    }
    // Interior lines.
    for y in (y0 + 1)..y1 {
        set(buf, split_x, y, "│", style);
    }
    for x in (x0 + 1)..x1 {
        set(buf, x, split_y, "─", style);
    }
    // Junctions.
    set(buf, x0, y0, "┌", style);
    set(buf, x1, y0, "┐", style);
    set(buf, x0, y1, "└", style);
    set(buf, x1, y1, "┘", style);
    set(buf, split_x, y0, "┬", style);
    set(buf, split_x, y1, "┴", style);
    set(buf, x0, split_y, "├", style);
    set(buf, x1, split_y, "┤", style);
    set(buf, split_x, split_y, "┼", style);

    let left_w = split_x.saturating_sub(x0 + 1);
    let right_w = x1.saturating_sub(split_x + 1);
    let top_h = split_y.saturating_sub(y0 + 1);
    let bot_h = y1.saturating_sub(split_y + 1);
    [
        Rect::new(x0 + 1, y0 + 1, left_w, top_h),            // TL
        Rect::new(split_x + 1, y0 + 1, right_w, top_h),      // TR
        Rect::new(x0 + 1, split_y + 1, left_w, bot_h),       // BL
        Rect::new(split_x + 1, split_y + 1, right_w, bot_h), // BR
    ]
}

/// Set a single cell to a string symbol with a style.
fn set(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) {
    if x < buf.area.right() && y < buf.area.bottom() {
        buf.set_string(x, y, s, style);
    }
}

/// Patch a single cell's style in place (keeps its glyph), bounds-guarded.
fn patch_cell_style(buf: &mut Buffer, x: u16, y: u16, style: Style) {
    if x < buf.area.right() && y < buf.area.bottom() {
        buf[(x, y)].set_style(style);
    }
}

/// Emphasize the frame around a focused pane (tui-rework 11, item 3): patch the
/// border cells surrounding the pane's content rect with `style` (bold), keeping
/// the light box glyphs. Palette-honest — a modifier, never a color/background
/// swap — so focus reads on any terminal.
fn emphasize_border(buf: &mut Buffer, inner: Rect, style: Style) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let x0 = inner.x.saturating_sub(1);
    let x1 = inner.x + inner.width; // right border column
    let y0 = inner.y.saturating_sub(1);
    let y1 = inner.y + inner.height; // bottom border row
    for x in x0..=x1 {
        patch_cell_style(buf, x, y0, style);
        patch_cell_style(buf, x, y1, style);
    }
    for y in y0..=y1 {
        patch_cell_style(buf, x0, y, style);
        patch_cell_style(buf, x1, y, style);
    }
}

/// Render one list pane: a title line, then scrollable entries with a
/// cursor-highlighted (glyph + color) selected entry.
#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    reason = "a list pane needs title, focus, entries, cursor, rect, and styling"
)]
fn render_list(
    title: &str,
    title_extra: &[ratatui::text::Span<'static>],
    focused: bool,
    entries: &[Vec<Line<'static>>],
    cursor: &mut Cursor,
    inner: Rect,
    buf: &mut Buffer,
    theme: &Theme,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // Title line inside the body: the pane title, then any extra spans (the
    // Problems pane carries the verdict counts here — tui-rework 09, item 2).
    // Focus is shown by a bold title (item 3) and a bold pane frame; unfocused
    // panes stay plain — no reverse-video, palette-honest on any terminal.
    let title_style = if focused { theme.title } else { Style::new() };
    let mut title_spans = vec![ratatui::text::Span::styled(title.to_string(), title_style)];
    title_spans.extend(title_extra.iter().cloned());
    let title_line = format::bound_line(&Line::from(title_spans), inner.width as usize);
    buf.set_line(inner.x, inner.y, &title_line, inner.width);

    let list_y = inner.y + 1;
    let list_h = inner.height.saturating_sub(1);
    if list_h == 0 {
        return;
    }

    let heights: Vec<u16> = entries.iter().map(|e| e.len() as u16).collect();
    // Keep the cursor entry in view.
    if cursor.index < cursor.scroll {
        cursor.scroll = cursor.index;
    }
    while cursor.scroll < cursor.index {
        let used: u16 = heights
            .get(cursor.scroll..=cursor.index)
            .map_or(0, |s| s.iter().sum());
        if used <= list_h {
            break;
        }
        cursor.scroll += 1;
    }

    let mut y = list_y;
    let end_y = list_y + list_h;
    let mut drawn = 0usize;
    for (i, entry) in entries.iter().enumerate().skip(cursor.scroll) {
        if y >= end_y {
            break;
        }
        let selected = focused && i == cursor.index;
        for (li, line) in entry.iter().enumerate() {
            if y >= end_y {
                break;
            }
            let bounded = format::bound_line(line, inner.width as usize);
            if selected {
                let hl = highlight_line(&bounded, theme.selection);
                buf.set_line(inner.x, y, &hl, inner.width);
                if li == 0 {
                    buf.set_string(inner.x, y, "▸", theme.selection);
                }
            } else {
                buf.set_line(inner.x, y, &bounded, inner.width);
            }
            y += 1;
        }
        drawn += 1;
    }
    cursor.visible = drawn.max(1);

    let counts = OverflowCounts {
        above: cursor.scroll,
        below: entries.len().saturating_sub(cursor.scroll + drawn),
    };
    render_overflow_counts(
        &counts,
        Rect::new(inner.x, list_y, inner.width, list_h),
        buf,
        theme.muted,
    );
}

/// Render a non-interactive detail pane: a title then wrapped content lines.
#[allow(
    clippy::cast_possible_truncation,
    reason = "detail line counts are small"
)]
fn render_detail(
    title: &str,
    focused: bool,
    lines: &[Line<'static>],
    inner: Rect,
    buf: &mut Buffer,
    theme: &Theme,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // Focus is shown by a bold title (item 3); unfocused panes stay plain.
    let title_style = if focused { theme.title } else { Style::new() };
    buf.set_string(inner.x, inner.y, truncate(title, inner.width), title_style);
    let end_y = inner.y + inner.height;
    for (i, line) in lines.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        if y >= end_y {
            break;
        }
        let bounded = format::bound_line(line, inner.width as usize);
        buf.set_line(inner.x, y, &bounded, inner.width);
    }
}

/// Re-style a line for the selection highlight (tui-rework 11, item 3):
/// patch each span with the selection modifier (bold — no background or
/// foreground swap, so grays stay grays on any terminal palette). The `▸`
/// gutter caret is drawn separately; no width padding is needed without a
/// filled background.
fn highlight_line(line: &Line<'static>, style: Style) -> Line<'static> {
    use ratatui::text::Span;
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|s| Span::styled(s.content.clone(), s.style.patch(style)))
        .collect();
    Line::from(spans)
}

/// Truncate a title to `width` columns.
fn truncate(s: &str, width: u16) -> String {
    format::truncate_to_width(s, width as usize)
}

/// Build the entry line-groups for a tree pane (one line per row).
fn tree_entries(
    rows: &[model::Row],
    width: u16,
    theme: &Theme,
    icons: &IconSet,
    now: DateTime<Utc>,
) -> Vec<Vec<Line<'static>>> {
    rows.iter()
        .map(|r| vec![render::tree_line(r, width as usize, theme, icons, now)])
        .collect()
}

/// The detail pane title, named for the focused tree (tui-rework 09, item 3):
/// `Details (Servers)` when the root/server tree drove the selection,
/// `Details (Sessions)` when the client/session tree did.
fn detail_title(app: &App<'_>) -> &'static str {
    if app.last_tree == Pane::SessionTree {
        " Details (Sessions)"
    } else {
        " Details (Servers)"
    }
}

/// The verdict counts that ride the Problems pane title (item 2): a leading gap
/// then the `● working` / `✗ N problems · M suggestions` spans.
fn problems_verdict_spans(app: &App<'_>, theme: &Theme) -> Vec<ratatui::text::Span<'static>> {
    let mut spans = vec![ratatui::text::Span::raw("   ".to_string())];
    spans.extend(render::verdict_spans(
        app.verdict,
        app.daemon_present(),
        theme,
    ));
    spans
}

/// Render one full frame.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "one draw covers header, grid, four panes, and footer; terminal coords are small"
)]
fn render_frame(f: &mut Frame<'_>, app: &mut App<'_>, rects: &mut PanelRects) {
    let area = f.area();
    *rects = PanelRects::default();
    if area.width < 24 || area.height < 8 {
        return;
    }

    let theme = app.theme;
    let icons = app.icons;
    // One injected clock per frame: every duration renders against it, so an
    // idle board is byte-identical between refreshes within a bucket (item 4).
    let now = app.render_now();
    let buf = f.buffer_mut();

    // No header strip (item 2): the verdict rides the Problems pane title and
    // the daemon identity/version/freshness ride the footer, so the grid runs
    // from the top row down to the footer.
    let footer_y = area.y + area.height - 1;
    if app.keybinds_expanded {
        // Expanded keybind panel occupies the lines just above the footer.
        let kb_h = KEYBINDS_EXPANDED_HEIGHT.min(area.height.saturating_sub(2));
        render_keybinds_content(
            Rect::new(area.x, footer_y.saturating_sub(kb_h), area.width, kb_h),
            buf,
            theme,
        );
    }
    let footer = render::footer_line(&app.snapshot, area.width as usize, theme, now);
    buf.set_line(area.x, footer_y, &footer, area.width);

    if !app.daemon_present() {
        let msg = "Waiting for daemon…";
        let w = UnicodeWidthStr::width(msg) as u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height / 2;
        buf.set_string(x, y, msg, theme.muted);
        return;
    }

    let grid = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));

    // Precompute detail lines + tree entries before mutating cursors.
    let detail_entity = app.detail_entity();
    let detail_lines = render::detail_lines(
        detail_entity.as_ref(),
        &app.snapshot,
        app.config.as_ref(),
        &app.findings,
        theme,
        now,
    );

    if area.width < NARROW_THRESHOLD {
        render_narrow(app, grid, buf, theme, icons, &detail_lines, now, rects);
        draw_action_overlay(app, area, buf, theme);
        return;
    }

    let split_x = grid.x + grid.width * LEFT_PCT / 100;
    let split_y = grid.y + grid.height / 2;
    let [tl, tr, bl, br] = draw_grid_borders(grid, split_x, split_y, buf, theme.border_unfocused);
    // Focus is a "bounding box" (item 3): the focused pane's frame is redrawn
    // emphasized (bold), no reverse-video anywhere.
    let focus_rect = match app.focus {
        Pane::RootTree => tl,
        Pane::Detail => tr,
        Pane::SessionTree => bl,
        Pane::Problems => br,
    };
    emphasize_border(buf, focus_rect, theme.border_focused);

    let root_entries = tree_entries(&app.root_rows, tl.width, theme, icons, now);
    let session_entries = tree_entries(&app.session_rows, bl.width, theme, icons, now);
    let mut problem_entries = render::problem_entries(&app.problem_rows, br.width as usize, theme);
    problem_entries.extend(render::pending_restart_entries(
        &app.pending_restarts,
        br.width as usize,
        theme,
    ));

    let problems_title_extra = problems_verdict_spans(app, theme);
    render_list(
        " Servers (by root)",
        &[],
        app.focus == Pane::RootTree,
        &root_entries,
        &mut app.root_cursor,
        tl,
        buf,
        theme,
    );
    render_detail(
        detail_title(app),
        app.focus == Pane::Detail,
        &detail_lines,
        tr,
        buf,
        theme,
    );
    render_list(
        " Sessions (by client)",
        &[],
        app.focus == Pane::SessionTree,
        &session_entries,
        &mut app.session_cursor,
        bl,
        buf,
        theme,
    );
    render_list(
        " Problems",
        &problems_title_extra,
        app.focus == Pane::Problems,
        &problem_entries,
        &mut app.problem_cursor,
        br,
        buf,
        theme,
    );

    rects.root = tl;
    rects.detail = tr;
    rects.session = bl;
    rects.problems = br;

    draw_action_overlay(app, area, buf, theme);
    draw_install_overlay(app, area, buf, theme);
}

/// Draw the guided-mutation consent overlay centered over the grid, when open.
fn draw_action_overlay(app: &App<'_>, area: Rect, buf: &mut Buffer, theme: &Theme) {
    if let Some(state) = &app.pending_action {
        let lines = render::action_overlay_lines(state, theme);
        draw_overlay(&lines, area, buf, theme);
    }
}

/// Draw the guided-install consent overlay centered over the grid, when open.
fn draw_install_overlay(app: &App<'_>, area: Rect, buf: &mut Buffer, theme: &Theme) {
    if let Some(state) = &app.pending_install {
        let lines = render::install_overlay_lines(state, theme);
        draw_overlay(&lines, area, buf, theme);
    }
}

/// Draw a bordered, background-cleared box of `lines` centered in `area`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "overlay dimensions are bounded by the terminal size"
)]
fn draw_overlay(lines: &[Line<'static>], area: Rect, buf: &mut Buffer, theme: &Theme) {
    let content_w = lines
        .iter()
        .map(|l| format::spans_width(&l.spans))
        .max()
        .unwrap_or(0);
    let max_inner = area.width.saturating_sub(4) as usize;
    let inner_w = content_w.clamp(28, max_inner.max(1)) as u16;
    let box_w = inner_w + 2;
    let box_h = lines.len() as u16 + 2;
    if box_w > area.width || box_h > area.height {
        return;
    }
    let x = area.x + (area.width - box_w) / 2;
    let y = area.y + (area.height - box_h) / 2;

    // Clear the box interior so the panes behind it do not bleed through.
    for yy in y..y + box_h {
        for xx in x..x + box_w {
            set(buf, xx, yy, " ", theme.text);
        }
    }
    // Border.
    for xx in x..x + box_w {
        set(buf, xx, y, "─", theme.accent);
        set(buf, xx, y + box_h - 1, "─", theme.accent);
    }
    for yy in y..y + box_h {
        set(buf, x, yy, "│", theme.accent);
        set(buf, x + box_w - 1, yy, "│", theme.accent);
    }
    set(buf, x, y, "┌", theme.accent);
    set(buf, x + box_w - 1, y, "┐", theme.accent);
    set(buf, x, y + box_h - 1, "└", theme.accent);
    set(buf, x + box_w - 1, y + box_h - 1, "┘", theme.accent);

    for (i, line) in lines.iter().enumerate() {
        buf.set_line(x + 1, y + 1 + i as u16, line, inner_w);
    }
}

/// Narrow degradation: stack the four panes full-width.
#[allow(
    clippy::too_many_arguments,
    reason = "narrow stacking needs the full render context"
)]
fn render_narrow(
    app: &mut App<'_>,
    grid: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    icons: &IconSet,
    detail_lines: &[Line<'static>],
    now: DateTime<Utc>,
    rects: &mut PanelRects,
) {
    let h = grid.height / 4;
    if h == 0 {
        return;
    }
    let tl = Rect::new(grid.x, grid.y, grid.width, h);
    let bl = Rect::new(grid.x, grid.y + h, grid.width, h);
    let tr = Rect::new(grid.x, grid.y + 2 * h, grid.width, h);
    let br = Rect::new(grid.x, grid.y + 3 * h, grid.width, grid.height - 3 * h);

    let root_entries = tree_entries(&app.root_rows, tl.width, theme, icons, now);
    let session_entries = tree_entries(&app.session_rows, bl.width, theme, icons, now);
    let mut problem_entries = render::problem_entries(&app.problem_rows, br.width as usize, theme);
    problem_entries.extend(render::pending_restart_entries(
        &app.pending_restarts,
        br.width as usize,
        theme,
    ));

    let problems_title_extra = problems_verdict_spans(app, theme);
    render_list(
        " Servers (by root)",
        &[],
        app.focus == Pane::RootTree,
        &root_entries,
        &mut app.root_cursor,
        tl,
        buf,
        theme,
    );
    render_list(
        " Sessions (by client)",
        &[],
        app.focus == Pane::SessionTree,
        &session_entries,
        &mut app.session_cursor,
        bl,
        buf,
        theme,
    );
    render_detail(
        detail_title(app),
        app.focus == Pane::Detail,
        detail_lines,
        tr,
        buf,
        theme,
    );
    render_list(
        " Problems",
        &problems_title_extra,
        app.focus == Pane::Problems,
        &problem_entries,
        &mut app.problem_cursor,
        br,
        buf,
        theme,
    );

    rects.root = tl;
    rects.session = bl;
    rects.detail = tr;
    rects.problems = br;
}

/// Main event loop: re-load the snapshot, render, handle input.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
    watch_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut rects = PanelRects::default();

    loop {
        terminal.draw(|f| render_frame(f, app, &mut rects))?;

        if app.quit {
            return Ok(());
        }

        let timeout = TICK_INTERVAL
            .checked_sub(last_tick.elapsed())
            .unwrap_or_default();

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                    handle_key(app, key.code);
                }
                Event::Mouse(mouse) => {
                    handle_mouse(app, mouse.kind, mouse.column, mouse.row, &rects);
                }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK_INTERVAL {
            if let Some(rx) = watch_rx {
                while rx.try_recv().is_ok() {}
            }
            app.reload();
            last_tick = Instant::now();
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::state_snapshot::{
        ClientInfo, DaemonSnapshot, LanguageActivity, LastMessage, ServerEntry, SessionEntry,
        Snapshot, Subagent,
    };
    use crate::tui::data::MockDataSource;
    use ratatui::backend::TestBackend;

    /// A fleet: 20+ servers across several roots, sessions on different hosts,
    /// all healthy except one broken server.
    fn fleet_fixture(one_broken: bool) -> Snapshot {
        let mut snap = Snapshot {
            daemon: DaemonSnapshot {
                instance_id: "daemon:test".to_string(),
                pid: 4242,
                version: env!("CATENARY_VERSION").to_string(),
                started_at: "2026-07-07T12:00:00Z".to_string(),
                generated_at: crate::state_snapshot::now_iso(),
            },
            ..Snapshot::default()
        };
        // 3 roots × 7 servers = 21 healthy instances.
        for r in 0..3 {
            for s in 0..7 {
                snap.servers.push(ServerEntry {
                    id: format!("srv{s}@/p/root{r}"),
                    language: format!("lang{s}"),
                    server: format!("srv{s}"),
                    scope_root: format!("/p/root{r}"),
                    state: "healthy".to_string(),
                    state_since: crate::state_snapshot::now_iso(),
                    ..ServerEntry::default()
                });
            }
        }
        if one_broken {
            snap.servers.push(ServerEntry {
                id: "julia-ls@/p/root0".to_string(),
                language: "julia".to_string(),
                server: "julia-ls".to_string(),
                scope_root: "/p/root0".to_string(),
                state: "failed".to_string(),
                state_since: crate::state_snapshot::now_iso(),
                last_message: Some(LastMessage {
                    level: "error".to_string(),
                    text: "initialize failed".to_string(),
                    at: crate::state_snapshot::now_iso(),
                }),
                ..ServerEntry::default()
            });
            // A tracked session touched a julia file, so julia is activity-live
            // and julia-ls's failure is an intent-broken Fatal (item 5). Without
            // this the failed instance would be quiet dormant inventory.
            snap.activity_languages.push(LanguageActivity {
                language: "julia".to_string(),
                root: "/p/root0".to_string(),
                files: vec!["src/main.jl".to_string()],
                file_count: 1,
            });
        }
        snap.sessions = vec![
            SessionEntry {
                id: "claude-abc".to_string(),
                client: ClientInfo {
                    name: "claude".to_string(),
                    version: None,
                },
                last_seen: crate::state_snapshot::now_iso(),
                subagents: vec![Subagent {
                    id: "agent-1".to_string(),
                    started_at: crate::state_snapshot::now_iso(),
                }],
                ..SessionEntry::default()
            },
            SessionEntry {
                id: "antigravity-xyz".to_string(),
                client: ClientInfo {
                    name: "antigravity".to_string(),
                    version: None,
                },
                last_seen: crate::state_snapshot::now_iso(),
                ..SessionEntry::default()
            },
        ];
        snap
    }

    /// The config the fleet fixture routes against: empty for the healthy
    /// fleet, `julia-ls → julia` for the broken variant (so the failed instance
    /// surfaces as an intent-broken `Fatal`).
    fn fleet_config(one_broken: bool) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        if one_broken {
            use crate::config::{LanguageConfig, ServerBinding, ServerDef};
            config.server.insert(
                "julia-ls".to_string(),
                ServerDef {
                    command: "julia-ls".to_string(),
                    ..ServerDef::default()
                },
            );
            config.language.insert(
                "julia".to_string(),
                LanguageConfig {
                    servers: Some(vec![ServerBinding::new("julia-ls")]),
                    ..Default::default()
                },
            );
        }
        config
    }

    fn render_to_string(
        snap: Snapshot,
        config: crate::config::Config,
        width: u16,
        height: u16,
    ) -> String {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = App::with_injected_config(
            &theme,
            &icons,
            PathBuf::from("/nonexistent"),
            config,
            Box::new(MockDataSource::new(snap)),
        )
        .expect("app");
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut rects = PanelRects::default();
        terminal
            .draw(|f| render_frame(f, &mut app, &mut rects))
            .expect("draw");
        buffer_to_string(terminal.backend().buffer())
    }

    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    /// Parse a fixed ISO instant for the injected render clock.
    fn at(iso: &str) -> DateTime<Utc> {
        crate::tui::format::parse_iso(iso).expect("iso")
    }

    /// A calm board with **fixed** timestamps so a render is deterministic under
    /// an injected `now`: one root with two healthy servers aged two minutes, a
    /// fresh claude session with a subagent, and a stale antigravity session.
    fn calm_fixture() -> Snapshot {
        let mut snap = Snapshot {
            daemon: DaemonSnapshot {
                instance_id: "daemon:test".to_string(),
                pid: 4242,
                version: env!("CATENARY_VERSION").to_string(),
                started_at: "2026-07-07T12:00:00Z".to_string(),
                generated_at: "2026-07-07T12:01:20Z".to_string(),
            },
            ..Snapshot::default()
        };
        for s in 0..2 {
            snap.servers.push(ServerEntry {
                id: format!("srv{s}@/p/root0"),
                language: format!("lang{s}"),
                server: format!("srv{s}"),
                scope_root: "/p/root0".to_string(),
                state: "healthy".to_string(),
                state_since: "2026-07-07T12:00:00Z".to_string(),
                ..ServerEntry::default()
            });
        }
        snap.sessions = vec![
            SessionEntry {
                id: "claude-abc".to_string(),
                client: ClientInfo {
                    name: "claude".to_string(),
                    version: None,
                },
                last_seen: "2026-07-07T12:01:10Z".to_string(),
                subagents: vec![Subagent {
                    id: "agent-1".to_string(),
                    started_at: "2026-07-07T12:00:00Z".to_string(),
                }],
                ..SessionEntry::default()
            },
            SessionEntry {
                id: "antigravity-xyz".to_string(),
                client: ClientInfo {
                    name: "antigravity".to_string(),
                    version: None,
                },
                last_seen: "2026-07-07T11:55:00Z".to_string(),
                ..SessionEntry::default()
            },
        ];
        snap
    }

    /// Build an app over a fixture with the render clock pinned to `now`.
    fn calm_app<'a>(
        theme: &'a Theme,
        icons: &'a IconSet,
        snap: Snapshot,
        now: DateTime<Utc>,
    ) -> App<'a> {
        let mut app = App::with_injected_config(
            theme,
            icons,
            PathBuf::from("/nonexistent"),
            crate::config::Config::default(),
            Box::new(MockDataSource::new(snap)),
        )
        .expect("app");
        app.inject_now(now);
        app
    }

    /// Draw an app to a buffer string at the given size.
    fn draw_app(app: &mut App<'_>, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut rects = PanelRects::default();
        terminal
            .draw(|f| render_frame(f, app, &mut rects))
            .expect("draw");
        buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn idle_board_is_byte_identical_within_a_quantization_bucket() {
        // Two renders 10s apart within the same minute: quantized durations mean
        // the board must be byte-for-byte identical — the idle-engine design law.
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = calm_app(&theme, &icons, calm_fixture(), at("2026-07-07T12:01:35Z"));
        // Expand the root so the servers' time-in-state renders in the tree.
        app.set_focus(Pane::RootTree);
        app.jump_home();
        app.activate();

        let out1 = draw_app(&mut app, 100, 30);
        app.inject_now(at("2026-07-07T12:01:45Z"));
        let out2 = draw_app(&mut app, 100, 30);

        assert_eq!(
            out1, out2,
            "an unchanged board within one bucket must be byte-identical:\n{out1}"
        );
        // And the durations really are quantized (no ticking seconds past 1m).
        assert!(
            out1.contains("just now"),
            "fresh snapshot freshness: {out1}"
        );
        assert!(
            out1.contains("last seen 6m"),
            "stale session quantized to minutes: {out1}"
        );
    }

    #[test]
    fn nothing_renders_past_width_at_narrow_size() {
        // A long, skewed daemon version overruns 60 columns; the footer must
        // truncate with `…` (item 1), never clip raw like `updated 4m21s ag`.
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut snap = calm_fixture();
        snap.daemon.version = "9.9.9-longbuild-gdeadbeefcafef00d-dirty".to_string();
        let mut app = calm_app(&theme, &icons, snap, at("2026-07-07T12:01:35Z"));

        let out = draw_app(&mut app, 60, 20);
        for line in out.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 60,
                "no line exceeds its area width: {line:?}"
            );
        }
        let footer = out.lines().last().expect("footer row");
        assert!(
            footer.contains('…'),
            "footer truncates with an ellipsis instead of clipping raw: {footer:?}"
        );
        assert!(
            footer.contains("? keys"),
            "the `? keys` hint survives; the daemon status yields first: {footer:?}"
        );
    }

    #[test]
    fn focus_and_selection_use_no_reverse_video() {
        use ratatui::style::Modifier;
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = calm_app(&theme, &icons, calm_fixture(), at("2026-07-07T12:01:35Z"));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut rects = PanelRects::default();
        terminal
            .draw(|f| render_frame(f, &mut app, &mut rects))
            .expect("draw");
        let buf = terminal.backend().buffer();

        // No cell anywhere reverse-videos — palette honesty (item 3).
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                assert!(
                    !buf[(x, y)].modifier.contains(Modifier::REVERSED),
                    "no reverse-video at ({x},{y})"
                );
            }
        }
        // The focused pane (RootTree, top-left) shows a bold frame corner.
        assert!(
            buf[(0, 0)].modifier.contains(Modifier::BOLD),
            "focused pane border is emphasized (bold)"
        );
        // The selected row keeps its `▸` gutter caret.
        assert!(
            buffer_to_string(buf).contains('▸'),
            "selected row keeps its caret gutter"
        );
    }

    #[test]
    fn footer_slims_to_keys_hint_and_daemon_status() {
        // The per-key hints left the footer (item 2): only `? keys` + daemon
        // status remain; the moved hints (e.g. `problems-only`) are gone from it.
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = calm_app(&theme, &icons, calm_fixture(), at("2026-07-07T12:01:35Z"));
        let out = draw_app(&mut app, 120, 30);
        let footer = out.lines().last().expect("footer row");
        assert!(
            footer.contains("? keys"),
            "keeps the discovery hint: {footer:?}"
        );
        assert!(
            footer.contains("daemon pid"),
            "keeps daemon status: {footer:?}"
        );
        assert!(
            !footer.contains("problems-only"),
            "per-key hints left the footer: {footer:?}"
        );
    }

    #[test]
    fn healthy_fleet_renders_quiet_verdict() {
        let out = render_to_string(fleet_fixture(false), fleet_config(false), 100, 30);
        assert!(out.contains("working"), "green verdict when healthy: {out}");
        assert!(out.contains("Servers (by root)"), "root tree title");
        assert!(out.contains("Problems"), "problems pane title");
    }

    #[test]
    fn healthy_fleet_collapses_roots_to_one_line_each() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let app = App::with_injected_config(
            &theme,
            &icons,
            PathBuf::from("/nonexistent"),
            fleet_config(false),
            Box::new(MockDataSource::new(fleet_fixture(false))),
        )
        .expect("app");
        // 21 servers over 3 roots → 3 collapsed root lines (density law).
        assert_eq!(app.root_rows.len(), 3);
    }

    #[test]
    fn broken_server_surfaces_in_problems_and_focuses() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = App::with_injected_config(
            &theme,
            &icons,
            PathBuf::from("/nonexistent"),
            fleet_config(true),
            Box::new(MockDataSource::new(fleet_fixture(true))),
        )
        .expect("app");
        // julia-ls failed → a problem with a fix-it.
        assert!(
            app.problem_rows
                .iter()
                .any(|p| p.message.contains("julia-ls")),
            "broken server is in the problems pane",
        );
        assert!(!app.verdict.is_working(), "verdict is not working");
        // Selecting it focuses the root tree on the owning server.
        app.set_focus(Pane::Problems);
        app.jump_home();
        app.activate();
        assert_eq!(
            app.focus,
            Pane::RootTree,
            "selection jumps to the root tree"
        );
        let idx = app.root_cursor.index;
        assert!(
            matches!(&app.root_rows[idx], model::Row::Server(s) if s.server == "julia-ls"),
            "cursor lands on the owning server",
        );
    }

    #[test]
    fn problems_pane_budget_fits_in_80x24() {
        // Verdict + a handful of problems must fit in 80×24 unscrolled.
        let out = render_to_string(fleet_fixture(true), fleet_config(true), 80, 24);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() <= 24, "fits the height: {}", lines.len());
        assert!(out.contains("julia-ls"), "the problem is visible: {out}");
    }

    #[test]
    fn narrow_layout_stacks_panes() {
        let out = render_to_string(fleet_fixture(true), fleet_config(true), 60, 40);
        assert!(out.contains("Servers (by root)"), "{out}");
        assert!(out.contains("Problems"), "{out}");
    }

    #[test]
    fn waiting_state_when_no_daemon() {
        let out = render_to_string(Snapshot::default(), fleet_config(false), 100, 24);
        assert!(out.contains("Waiting for daemon"), "{out}");
    }

    #[test]
    fn watcher_fires_on_rename_into_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let (_watcher, rx) = start_state_watcher(&path).expect("watcher");
        let tmp = dir.path().join("state.json.tmp");
        std::fs::write(&tmp, "{}").expect("write tmp");
        std::fs::rename(&tmp, &path).expect("rename");
        let got = rx.recv_timeout(Duration::from_secs(5));
        assert!(got.is_ok(), "watcher signals on rename to state.json");
    }
}
