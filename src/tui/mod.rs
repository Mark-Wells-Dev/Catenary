// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for monitoring sessions and tailing events.
//!
//! Renders a quadrant layout: Workspaces (upper-left), a collapsible
//! Keybinds panel (lower-left), and a full-height message stream (right).

pub mod app;
pub mod data;
pub mod format;
pub mod hints;
pub mod icons;
pub mod popup;
pub mod scope;
pub mod scrollbar;
pub mod sidebar;
pub mod stream;
pub mod theme;

pub use app::App;
pub use data::DataSource;

use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::Watcher;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Tabs, Widget};
use tracing::info;

use crate::config::IconConfig;

use self::app::{EffectiveLayout, FocusRegion};
use self::data::SqliteDataSource;
use self::hints::{KEYBINDS_EXPANDED_HEIGHT, render_keybinds_content};
use self::icons::IconSet;
use self::popup::render_server_detail;
use self::sidebar::render_workspaces;
use self::stream::render_stream;
use self::theme::Theme;

/// Tick interval for the event loop.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Minimum sidebar width percentage.
const MIN_SIDEBAR_PCT: u16 = 10;

/// Maximum sidebar width percentage.
const MAX_SIDEBAR_PCT: u16 = 90;

/// Start a file watcher on the WAL file's parent directory.
///
/// Watches the parent directory (non-recursive) because the WAL file may not
/// exist yet (`SQLite` creates it on first write). Events are filtered to the
/// WAL filename and coalesced into a single `()` signal.
fn start_wal_watcher(db_path: &Path) -> Result<(notify::RecommendedWatcher, mpsc::Receiver<()>)> {
    let wal_name = {
        let mut name = db_path.file_name().unwrap_or_default().to_os_string();
        name.push("-wal");
        name
    };

    let (tx, rx) = mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let matches_wal = event.paths.iter().any(|p| p.file_name() == Some(&wal_name));
            if matches_wal {
                let _ = tx.send(());
            }
        }
    })?;

    let watch_dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    watcher.watch(watch_dir, notify::RecursiveMode::NonRecursive)?;

    Ok((watcher, rx))
}

/// Run the interactive TUI with the live data source.
///
/// # Errors
///
/// Returns an error if terminal setup fails or session data cannot be read.
pub fn run(icon_config: IconConfig) -> Result<()> {
    let data = Box::new(SqliteDataSource::new()?);
    let db_path = crate::db::db_path();

    let wal_watcher = match start_wal_watcher(&db_path) {
        Ok((watcher, rx)) => Some((watcher, rx)),
        Err(e) => {
            info!("WAL watcher unavailable, falling back to polling: {e}");
            None
        }
    };

    // Hold _watcher to keep it alive; extract rx for the event loop.
    let (_watcher, wal_rx) = match wal_watcher {
        Some((w, rx)) => (Some(w), Some(rx)),
        None => (None, None),
    };

    run_with_data_and_watcher(icon_config, data, wal_rx.as_ref())
}

/// Run the interactive TUI with an optional WAL watcher.
fn run_with_data_and_watcher(
    icon_config: IconConfig,
    data: Box<dyn DataSource>,
    wal_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let theme = Theme::new();
    let icons = IconSet::from_config(icon_config);

    let mut app = App::new(&theme, &icons, data)?;

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app, wal_rx);

    // Terminal teardown.
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Tab label names for the left-column stack.
const LEFT_TAB_NAMES: &[&str] = &["Workspaces", "Keybinds"];
/// Tab label names for the full-width stack (includes Traffic).
const FULL_TAB_NAMES: &[&str] = &["Workspaces", "Keybinds", "Traffic"];

/// Stored panel rectangles and hit maps for mouse dispatch.
struct PanelLayout {
    workspaces: Rect,
    keybinds: Rect,
    stream: Rect,
    /// Tab bar area (empty rect when in Quadrant mode).
    tab_bar: Rect,
    /// Number of tabs rendered in the tab bar.
    tab_count: usize,
    /// Server message detail panel (present when popup is open).
    detail: Option<Rect>,
    /// Inner height of the detail panel (for scroll clamping).
    detail_height: usize,
    /// Terminal row → `workspace_rows` index.
    workspace_hits: Vec<(u16, usize)>,
    /// Column at the right edge of the left panel (divider hit target).
    divider_col: u16,
    /// Total terminal width for percentage computation during drag.
    total_width: u16,
    /// Inner height of the stream panel (excludes borders and search bar).
    stream_inner_height: usize,
}

/// Handle a key event, dispatching to global or focus-specific handlers.
#[allow(
    clippy::too_many_lines,
    reason = "key dispatch covers popup, search, global, and panel-specific handlers"
)]
fn handle_key(
    app: &mut App<'_>,
    code: KeyCode,
    modifiers: KeyModifiers,
    viewport_height: usize,
    detail_height: usize,
) {
    // Detail panel captures all keys when open.
    if let Some(ref mut popup) = app.popup {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_popup(),
            KeyCode::Char('j') | KeyCode::Down => popup.scroll_down(1, detail_height),
            KeyCode::Char('k') | KeyCode::Up => popup.scroll_up(1),
            KeyCode::PageDown => popup.scroll_down(detail_height / 2, detail_height),
            KeyCode::PageUp => popup.scroll_up(detail_height / 2),
            KeyCode::Char('y') => {
                if let Some(text) = popup.yank_text() {
                    osc52_copy(&text);
                }
            }
            _ => {}
        }
        return;
    }

    // Search input mode intercepts all keys.
    if app.search_active {
        handle_search_input(app, code, viewport_height);
        return;
    }

    // Global keys (always active regardless of focus).
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') => app.toggle_keybinds(),
        KeyCode::Char('b') => app.cycle_left_tab(),
        KeyCode::Tab => {
            app.stream.exit_visual();
            app.sidebar.exit_visual();
            app.cycle_focus();
        }
        KeyCode::BackTab => {
            app.stream.exit_visual();
            app.sidebar.exit_visual();
            app.cycle_focus_back();
        }
        _ => match app.focus {
            FocusRegion::Workspaces => {
                let visible = viewport_height.saturating_sub(1);
                match code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.sidebar.cursor_down(1, visible);
                    }
                    KeyCode::Char('k') | KeyCode::Up => app.sidebar.cursor_up(1, visible),
                    KeyCode::Char('h') | KeyCode::Left => {
                        app.sidebar.hscroll_left();
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        app.sidebar.hscroll_right();
                    }
                    KeyCode::Enter => {
                        // Enter on a root: toggle expansion.
                        // Enter on a server: open popup.
                        if app.sidebar.cursor_server_index().is_some() {
                            app.open_server_popup();
                        } else {
                            app.sidebar.toggle_expanded();
                        }
                    }
                    KeyCode::Char('f') => {
                        app.toggle_workspace_selection();
                    }
                    KeyCode::Char(' ') => {
                        if modifiers.contains(KeyModifiers::SHIFT) {
                            if !app.sidebar.in_visual() {
                                app.sidebar.start_visual();
                            }
                        } else if app.sidebar.in_visual() {
                            app.sidebar.exit_visual();
                        } else {
                            app.sidebar.start_visual();
                        }
                    }
                    KeyCode::Char('y') => {
                        if let Some(text) = app.sidebar.yank_text() {
                            osc52_copy(&text);
                        }
                        app.sidebar.exit_visual();
                    }
                    KeyCode::Esc => {
                        app.sidebar.exit_visual();
                    }
                    _ => {}
                }
            }
            FocusRegion::Keybinds => {
                // No navigation inside keybinds panel.
            }
            FocusRegion::Stream => {
                handle_stream_key(app, code, modifiers, viewport_height);
            }
        },
    }
}

/// Handle a key event when the stream is focused.
fn handle_stream_key(
    app: &mut App<'_>,
    code: KeyCode,
    modifiers: KeyModifiers,
    viewport_height: usize,
) {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.stream.cursor_down(1, viewport_height);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.stream.cursor_up(1);
        }
        KeyCode::Enter if !app.stream.in_visual() => {
            app.stream.toggle_expansion();
        }
        KeyCode::Char(' ') => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                if !app.stream.in_visual() {
                    app.stream.start_visual();
                }
            } else if app.stream.in_visual() {
                app.stream.exit_visual();
            } else {
                app.stream.start_visual();
            }
        }
        KeyCode::Esc => {
            if app.stream.in_visual() {
                app.stream.exit_visual();
            } else {
                app.stream.clear_search();
            }
        }
        KeyCode::Char('y') => {
            if let Some(text) = app.stream.yank_text(app.icons) {
                osc52_copy(&text);
            }
            app.stream.exit_visual();
        }
        KeyCode::Char('/') => {
            app.search_active = true;
            app.search_input.clear();
        }
        KeyCode::Char('n') => {
            app.stream.search_next(viewport_height);
        }
        KeyCode::Char('N') => {
            app.stream.search_prev(viewport_height);
        }
        KeyCode::PageDown => {
            app.stream.scroll_down(viewport_height / 2, viewport_height);
        }
        KeyCode::PageUp => {
            app.stream.scroll_up(viewport_height / 2);
        }
        KeyCode::Home => {
            app.jump_to_beginning();
        }
        KeyCode::End => {
            app.stream.pin_to_bottom(viewport_height);
        }
        _ => {}
    }
}

/// Handle a key event during search input mode.
fn handle_search_input(app: &mut App<'_>, code: KeyCode, viewport_height: usize) {
    match code {
        KeyCode::Char(c) => {
            app.search_input.push(c);
            app.stream
                .set_search(Some(app.search_input.clone()), app.icons);
        }
        KeyCode::Backspace => {
            app.search_input.pop();
            if app.search_input.is_empty() {
                app.stream.clear_search();
            } else {
                app.stream
                    .set_search(Some(app.search_input.clone()), app.icons);
            }
        }
        KeyCode::Enter => {
            app.search_active = false;
            if app.search_input.is_empty() {
                app.stream.clear_search();
            }
        }
        KeyCode::Esc => {
            app.search_active = false;
            app.search_input.clear();
            app.stream.clear_search();
        }
        KeyCode::Down => {
            app.stream.search_next(viewport_height);
        }
        KeyCode::Up => {
            app.stream.search_prev(viewport_height);
        }
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
    reason = "base64 index is always 0..63, safe for usize; byte-to-char is ASCII"
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
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Handle a mouse event, dispatching to the correct panel based on position.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "terminal coordinates are always small; mouse dispatch covers many panel regions"
)]
fn handle_mouse(
    app: &mut App<'_>,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    layout: &PanelLayout,
    viewport_height: usize,
) {
    // Detail panel mouse handling.
    if app.popup.is_some() {
        let in_detail = layout
            .detail
            .is_some_and(|r| r.contains((column, row).into()));
        if in_detail {
            if let Some(ref mut popup) = app.popup {
                match kind {
                    MouseEventKind::ScrollUp => popup.scroll_up(3),
                    MouseEventKind::ScrollDown => popup.scroll_down(3, layout.detail_height),
                    _ => {}
                }
            }
            return;
        }
        if matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
            app.close_popup();
        }
    }

    // ── Divider drag ───────────────────────────────────────────
    if matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
        app.dragging_divider = layout.total_width > 0 && column.abs_diff(layout.divider_col) <= 1;
        if app.dragging_divider {
            return;
        }
    }
    if matches!(kind, MouseEventKind::Up(MouseButton::Left)) {
        app.dragging_divider = false;
    }
    if app.dragging_divider {
        if kind == MouseEventKind::Drag(MouseButton::Left) && layout.total_width > 0 {
            let raw = (u32::from(column) * 100 / u32::from(layout.total_width)) as u16;
            app.sidebar_pct = raw.clamp(MIN_SIDEBAR_PCT, MAX_SIDEBAR_PCT);
        }
        return;
    }

    // ── Normal panel dispatch ──────────────────────────────────
    let pos = (column, row);
    let in_tab_bar = layout.tab_bar.contains(pos.into());
    let in_workspaces = layout.workspaces.contains(pos.into());
    let in_keybinds = layout.keybinds.contains(pos.into());
    let in_stream = layout.stream.contains(pos.into());

    match kind {
        MouseEventKind::ScrollUp => {
            if in_workspaces {
                app.sidebar.cursor_up(3, viewport_height);
            } else if in_stream {
                app.stream.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_workspaces {
                app.sidebar.cursor_down(3, viewport_height);
            } else if in_stream {
                app.stream.scroll_down(3, viewport_height);
            }
        }
        MouseEventKind::ScrollLeft if in_workspaces => {
            app.sidebar.hscroll_left();
        }
        MouseEventKind::ScrollRight if in_workspaces => {
            app.sidebar.hscroll_right();
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if in_tab_bar && layout.tab_count > 0 {
                let relative_x = column.saturating_sub(layout.tab_bar.x);
                let clicked =
                    (relative_x as usize * layout.tab_count) / layout.tab_bar.width.max(1) as usize;
                let clicked = clicked.min(layout.tab_count - 1);
                app.set_left_tab(clicked);
            } else if in_workspaces {
                app.stream.exit_visual();
                app.sidebar.exit_visual();
                app.focus = FocusRegion::Workspaces;
                if let Some(&(_, idx)) = layout.workspace_hits.iter().find(|(r, _)| *r == row) {
                    app.sidebar.cursor = idx;
                    app.toggle_workspace_selection();
                }
            } else if in_keybinds {
                app.stream.exit_visual();
                app.sidebar.exit_visual();
                if app.effective == EffectiveLayout::Quadrant && !app.keybinds_expanded {
                    app.keybinds_expanded = true;
                }
                app.focus = FocusRegion::Keybinds;
            } else if in_stream {
                app.sidebar.exit_visual();
                app.focus = FocusRegion::Stream;
                if app.stream.in_visual() {
                    app.stream.exit_visual();
                } else {
                    let inner_top = layout.stream.y as usize + 1; // +1 for top border
                    let relative_row = (row as usize).saturating_sub(inner_top);
                    let stream_row = app.stream.scroll_position + relative_row;
                    if stream_row < app.stream.display_rows.len() {
                        app.stream.cursor = stream_row;
                        app.stream.auto_scroll = false;
                        app.stream.toggle_expansion();
                    }
                }
            }
        }
        _ => {}
    }
}

/// Build a `Block` frame for a panel, styling the border based on focus.
fn panel_block<'a>(title: &'a str, focused: bool, theme: &'a Theme, borders: Borders) -> Block<'a> {
    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let title_style = if focused { theme.title } else { theme.muted };
    Block::default()
        .borders(borders)
        .border_style(border_style)
        .title(Span::styled(title, title_style))
}

/// Render a horizontal separator row in the left column.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
fn render_left_separator(
    y: u16,
    x: u16,
    width: u16,
    title: &str,
    focused: bool,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let title_style = if focused { theme.title } else { theme.muted };

    buf.set_string(x, y, "├", border_style);

    for col in (x + 1)..(x + width) {
        buf.set_string(col, y, "─", border_style);
    }

    if !title.is_empty() && width > 2 {
        let max = (width - 1) as usize;
        let truncated: String = title.chars().take(max).collect();
        buf.set_string(x + 1, y, &truncated, title_style);
    }
}

/// Render a horizontal separator row in the right column.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_arguments,
    reason = "terminal coordinates are always small; separator needs position, content, style, and border flag"
)]
fn render_right_separator(
    y: u16,
    x: u16,
    width: u16,
    title: &str,
    focused: bool,
    theme: &Theme,
    buf: &mut Buffer,
    has_left_border: bool,
) {
    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let title_style = if focused { theme.title } else { theme.muted };

    for col in x..(x + width) {
        buf.set_string(col, y, "─", border_style);
    }

    if has_left_border && width > 0 {
        buf.set_string(x, y, "├", border_style);
    }

    if width > 0 {
        buf.set_string(x + width - 1, y, "┤", border_style);
    }

    let title_offset = u16::from(has_left_border);
    if !title.is_empty() && width > 2 + title_offset {
        let max = (width - 2 - title_offset) as usize;
        let truncated: String = title.chars().take(max).collect();
        buf.set_string(x + 1 + title_offset, y, &truncated, title_style);
    }
}

/// Render the vertical divider column between the left panels and the
/// right side, using box-drawing intersection characters.
fn render_divider_col(
    col: u16,
    top: u16,
    bottom: u16,
    left_seps: &[u16],
    right_seps: &[u16],
    style: Style,
    buf: &mut Buffer,
) {
    for y in top..=bottom {
        let has_left = left_seps.contains(&y);
        let has_right = right_seps.contains(&y);
        let ch = if y == top {
            "┬"
        } else if y == bottom {
            "┴"
        } else if has_left && has_right {
            "┼"
        } else if has_left {
            "┤"
        } else if has_right {
            "├"
        } else {
            "│"
        };
        buf.set_string(col, y, ch, style);
    }
}

/// Render the search bar at the bottom of the stream panel.
#[allow(
    clippy::cast_possible_truncation,
    reason = "search input length is always small"
)]
fn render_search_bar(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    input_active: bool,
    input: &str,
    status: Option<&str>,
) {
    if area.width < 4 {
        return;
    }

    let mut spans = vec![Span::styled("/", theme.accent)];
    if input_active {
        spans.push(Span::raw(input.to_string()));
    } else {
        spans.push(Span::styled(input.to_string(), theme.text));
    }

    if let Some(status) = status {
        let prefix_len = 1 + input.len();
        let status_len = status.len() + 1;
        let gap = (area.width as usize).saturating_sub(prefix_len + status_len);
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(status.to_string(), theme.muted));
        }
    }

    let line = ratatui::text::Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}

/// Render a horizontal tab bar with the active tab highlighted.
fn render_tab_bar(names: &[&str], active: usize, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let titles: Vec<Line<'_>> = names.iter().map(|n| Line::from(*n)).collect();
    Tabs::new(titles)
        .select(active)
        .highlight_style(theme.title)
        .style(theme.muted)
        .divider("│")
        .render(area, buf);
}

/// Render Workspaces into `panel_rect`, returning the hit map.
fn render_workspaces_panel(
    app: &App<'_>,
    panel_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
    borders: Borders,
    title: &str,
) {
    let focused = app.focus == FocusRegion::Workspaces;
    let block = panel_block(title, focused, app.theme, borders);
    let inner = block.inner(panel_rect);
    block.render(panel_rect, buf);
    layout.workspace_hits = render_workspaces(&app.sidebar, inner, buf, app.theme, focused);
    layout.workspaces = panel_rect;
}

/// Render the Keybinds panel at full height (for stacked/full-stack mode).
fn render_keybinds_panel(
    app: &App<'_>,
    panel_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
    borders: Borders,
    title: &str,
) {
    let focused = app.focus == FocusRegion::Keybinds;
    let block = panel_block(title, focused, app.theme, borders);
    let inner = block.inner(panel_rect);
    block.render(panel_rect, buf);
    render_keybinds_content(inner, buf, app.theme);
    layout.keybinds = panel_rect;
}

/// Render the right side: optional server detail panel above the Traffic panel.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
fn render_right_side(
    app: &mut App<'_>,
    right_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
    right_borders: Borders,
) -> (Option<(u16, u16)>, Vec<u16>) {
    let mut right_seps = Vec::new();

    let (detail_rect, sep_rect, messages_rect, detail_borders, messages_borders) =
        if app.popup.is_some() {
            let v = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                ])
                .split(right_rect);
            right_seps.push(v[1].y);
            (
                Some(v[0]),
                Some(v[1]),
                v[2],
                (right_borders | Borders::TOP) - Borders::BOTTOM,
                (right_borders | Borders::BOTTOM) - Borders::TOP,
            )
        } else {
            (None, None, right_rect, Borders::NONE, right_borders)
        };

    if let (Some(detail_area), Some(popup)) = (detail_rect, &mut app.popup) {
        render_server_detail(popup, detail_area, buf, app.theme, true, detail_borders);
        let border_rows = u16::from(detail_borders.contains(Borders::TOP))
            + u16::from(detail_borders.contains(Borders::BOTTOM));
        layout.detail_height = detail_area.height.saturating_sub(border_rows) as usize;
    }
    layout.detail = detail_rect;

    if let Some(sep) = sep_rect {
        let focused = app.focus == FocusRegion::Stream;
        let title = if app.stream.in_visual() {
            " Traffic  VISUAL "
        } else {
            " Traffic "
        };
        let has_left = right_borders.contains(Borders::LEFT);
        render_right_separator(
            sep.y, sep.x, sep.width, title, focused, app.theme, buf, has_left,
        );
    }

    let cursor = render_messages_panel(app, messages_rect, buf, layout, messages_borders);
    (cursor, right_seps)
}

/// Render the Traffic stream panel into `panel_rect`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "search input length is always small"
)]
fn render_messages_panel(
    app: &App<'_>,
    panel_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
    borders: Borders,
) -> Option<(u16, u16)> {
    let focused = app.focus == FocusRegion::Stream;
    let title = if borders.contains(Borders::TOP) {
        if app.stream.in_visual() {
            " Traffic  VISUAL "
        } else {
            " Traffic "
        }
    } else {
        ""
    };
    let block = panel_block(title, focused, app.theme, borders);
    let inner = block.inner(panel_rect);
    block.render(panel_rect, buf);

    let show_search = app.search_active || app.stream.has_search();
    let (stream_area, search_bar_area) = if show_search && inner.height > 1 {
        let content_height = inner.height - 1;
        (
            Rect {
                height: content_height,
                ..inner
            },
            Some(Rect {
                y: inner.y + content_height,
                height: 1,
                ..inner
            }),
        )
    } else {
        (inner, None)
    };

    render_stream(&app.stream, stream_area, buf, app.theme, app.icons);
    layout.stream = panel_rect;
    layout.stream_inner_height = stream_area.height as usize;

    if let Some(bar) = search_bar_area {
        render_search_bar(
            bar,
            buf,
            app.theme,
            app.search_active,
            &app.search_input,
            app.stream.search_status().as_deref(),
        );
        if app.search_active {
            let cursor_x = bar.x + 1 + app.search_input.len() as u16;
            return Some((cursor_x.min(bar.x + bar.width - 1), bar.y));
        }
    }
    None
}

/// Main event loop — renders layout based on effective mode, handles input.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "render loop with three layout modes + empty state in one draw closure; terminal widths are always small"
)]
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
    wal_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut layout = PanelLayout {
        workspaces: Rect::default(),
        keybinds: Rect::default(),
        stream: Rect::default(),
        tab_bar: Rect::default(),
        tab_count: 0,
        detail: None,
        detail_height: 0,
        workspace_hits: Vec::new(),
        divider_col: 0,
        total_width: 0,
        stream_inner_height: 0,
    };

    loop {
        let size = terminal.size()?;
        let stream_height = if layout.stream_inner_height > 0 {
            layout.stream_inner_height
        } else {
            (size.height as usize).saturating_sub(2)
        };
        app.stream.recompute_search_if_dirty(app.icons);
        app.update_effective(size.width, size.height);
        app.stream.apply_auto_scroll(stream_height);

        terminal.draw(|f| {
            let area = f.area();

            // Empty state.
            if app.sidebar.workspaces.is_empty()
                && app.sidebar.entries.is_empty()
                && app.sidebar.servers.is_empty()
                && app.sidebar.dead_servers.is_empty()
                && app.stream.entries.is_empty()
            {
                let msg = "Waiting for connections\u{2026}";
                let msg_width = unicode_width::UnicodeWidthStr::width(msg) as u16;
                let x = area.x + area.width.saturating_sub(msg_width) / 2;
                let y = area.y + area.height / 2;
                f.buffer_mut().set_string(x, y, msg, app.theme.muted);
                layout.workspace_hits.clear();
                layout.workspaces = Rect::default();
                layout.keybinds = Rect::default();
                layout.stream = Rect::default();
                layout.tab_bar = Rect::default();
                layout.tab_count = 0;
                layout.divider_col = 0;
                layout.total_width = 0;
                layout.detail = None;
                layout.detail_height = 0;
                return;
            }

            // Reset per-frame state.
            layout.workspace_hits.clear();
            layout.workspaces = Rect::default();
            layout.keybinds = Rect::default();
            layout.stream = Rect::default();
            layout.tab_bar = Rect::default();
            layout.tab_count = 0;
            layout.divider_col = 0;
            layout.total_width = 0;

            let mut search_cursor: Option<(u16, u16)> = None;

            match app.effective {
                // ── Quadrant: Workspaces + Keybinds left, Traffic right
                EffectiveLayout::Quadrant => {
                    let h_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(app.sidebar_pct),
                            Constraint::Length(1),
                            Constraint::Fill(1),
                        ])
                        .split(area);

                    let left = h_chunks[0];
                    let divider_rect = h_chunks[1];
                    let right = h_chunks[2];

                    layout.divider_col = divider_rect.x;
                    layout.total_width = area.width;

                    let keybinds_height = if app.keybinds_expanded {
                        KEYBINDS_EXPANDED_HEIGHT + 1
                    } else {
                        1 // just the BOTTOM border row
                    };

                    let v_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Fill(1),
                            Constraint::Length(1),
                            Constraint::Length(keybinds_height),
                        ])
                        .split(left);

                    let workspaces_rect = v_chunks[0];
                    let sep = v_chunks[1];
                    let keybinds_rect = v_chunks[2];

                    // Workspaces: TOP + LEFT.
                    render_workspaces_panel(
                        app,
                        workspaces_rect,
                        f.buffer_mut(),
                        &mut layout,
                        Borders::TOP | Borders::LEFT,
                        " Workspaces ",
                    );

                    // Separator: Keybinds title.
                    let keybinds_focused = app.focus == FocusRegion::Keybinds;
                    let keybinds_title = if app.keybinds_expanded {
                        " Keybinds  ? "
                    } else {
                        " Keybinds  ? to expand "
                    };
                    render_left_separator(
                        sep.y,
                        sep.x,
                        sep.width,
                        keybinds_title,
                        keybinds_focused,
                        app.theme,
                        f.buffer_mut(),
                    );

                    // Keybinds: BOTTOM + LEFT.
                    let keybinds_block = panel_block(
                        "",
                        keybinds_focused,
                        app.theme,
                        Borders::BOTTOM | Borders::LEFT,
                    );
                    let keybinds_inner = keybinds_block.inner(keybinds_rect);
                    keybinds_block.render(keybinds_rect, f.buffer_mut());
                    if app.keybinds_expanded {
                        render_keybinds_content(keybinds_inner, f.buffer_mut(), app.theme);
                    }
                    layout.keybinds = Rect {
                        y: sep.y,
                        height: sep.height + keybinds_rect.height,
                        ..keybinds_rect
                    };

                    // Right side: Traffic (+ optional detail panel).
                    let right_borders = Borders::TOP | Borders::RIGHT | Borders::BOTTOM;
                    let (cursor, right_seps) =
                        render_right_side(app, right, f.buffer_mut(), &mut layout, right_borders);
                    search_cursor = cursor;

                    // Vertical divider.
                    let left_seps = [sep.y];
                    render_divider_col(
                        divider_rect.x,
                        divider_rect.y,
                        divider_rect.y + divider_rect.height.saturating_sub(1),
                        &left_seps,
                        &right_seps,
                        app.theme.border_unfocused,
                        f.buffer_mut(),
                    );
                }

                // ── Stacked: tab bar + active panel left, Traffic right
                EffectiveLayout::Stacked => {
                    let h_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(app.sidebar_pct),
                            Constraint::Length(1),
                            Constraint::Fill(1),
                        ])
                        .split(area);

                    let left = h_chunks[0];
                    let divider_rect = h_chunks[1];
                    let right = h_chunks[2];

                    layout.divider_col = divider_rect.x;
                    layout.total_width = area.width;

                    let v_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(1), Constraint::Fill(1)])
                        .split(left);

                    let tab_bar_rect = v_chunks[0];
                    let panel_rect = v_chunks[1];

                    render_tab_bar(
                        LEFT_TAB_NAMES,
                        app.active_left_tab,
                        tab_bar_rect,
                        f.buffer_mut(),
                        app.theme,
                    );
                    layout.tab_bar = tab_bar_rect;
                    layout.tab_count = LEFT_TAB_NAMES.len();

                    let stacked_borders = Borders::TOP | Borders::LEFT | Borders::BOTTOM;
                    match app.active_left_tab {
                        0 => render_workspaces_panel(
                            app,
                            panel_rect,
                            f.buffer_mut(),
                            &mut layout,
                            stacked_borders,
                            " Workspaces ",
                        ),
                        _ => render_keybinds_panel(
                            app,
                            panel_rect,
                            f.buffer_mut(),
                            &mut layout,
                            stacked_borders,
                            " Keybinds ",
                        ),
                    }

                    // Right side.
                    let right_borders = Borders::TOP | Borders::RIGHT | Borders::BOTTOM;
                    let (cursor, right_seps) =
                        render_right_side(app, right, f.buffer_mut(), &mut layout, right_borders);
                    search_cursor = cursor;

                    let left_seps = [panel_rect.y];
                    render_divider_col(
                        divider_rect.x,
                        divider_rect.y,
                        divider_rect.y + divider_rect.height.saturating_sub(1),
                        &left_seps,
                        &right_seps,
                        app.theme.border_unfocused,
                        f.buffer_mut(),
                    );
                }

                // ── FullStack: tab bar + one panel full-width ──────
                EffectiveLayout::FullStack => {
                    let v_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(1), Constraint::Fill(1)])
                        .split(area);

                    let tab_bar_rect = v_chunks[0];
                    let panel_rect = v_chunks[1];

                    let visible_tab = if matches!(app.focus, FocusRegion::Stream) {
                        2
                    } else {
                        app.active_left_tab
                    };

                    render_tab_bar(
                        FULL_TAB_NAMES,
                        visible_tab,
                        tab_bar_rect,
                        f.buffer_mut(),
                        app.theme,
                    );
                    layout.tab_bar = tab_bar_rect;
                    layout.tab_count = FULL_TAB_NAMES.len();

                    match visible_tab {
                        0 => render_workspaces_panel(
                            app,
                            panel_rect,
                            f.buffer_mut(),
                            &mut layout,
                            Borders::ALL,
                            " Workspaces ",
                        ),
                        1 => render_keybinds_panel(
                            app,
                            panel_rect,
                            f.buffer_mut(),
                            &mut layout,
                            Borders::ALL,
                            " Keybinds ",
                        ),
                        _ => {
                            let (cursor, _) = render_right_side(
                                app,
                                panel_rect,
                                f.buffer_mut(),
                                &mut layout,
                                Borders::ALL,
                            );
                            search_cursor = cursor;
                        }
                    }
                }
            }

            if let Some(pos) = search_cursor {
                f.set_cursor_position(pos);
            }
        })?;

        if app.quit {
            return Ok(());
        }

        let timeout = TICK_INTERVAL
            .checked_sub(last_tick.elapsed())
            .unwrap_or_default();

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    handle_key(
                        app,
                        key.code,
                        key.modifiers,
                        stream_height,
                        layout.detail_height,
                    );
                    app.fetch_page_if_needed();
                }
                Event::Mouse(mouse) => {
                    handle_mouse(
                        app,
                        mouse.kind,
                        mouse.column,
                        mouse.row,
                        &layout,
                        stream_height,
                    );
                    app.fetch_page_if_needed();
                }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK_INTERVAL {
            if let Some(rx) = wal_rx {
                while rx.try_recv().is_ok() {}
            }
            app.drain_tail();
            app.refresh_sessions();
            app.refresh_servers();
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
    use std::collections::HashMap;

    use crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    use super::*;
    use crate::config::IconConfig;
    use crate::tui::data::MockDataSource;

    fn make_app<'a>(theme: &'a Theme, icons: &'a IconSet) -> App<'a> {
        let data: Box<dyn DataSource> = Box::new(MockDataSource {
            sessions: Vec::new(),
            messages: HashMap::new(),
            tail_messages: HashMap::new(),
            server_statuses: Vec::new(),
            server_noise: Vec::new(),
        });
        App::new(theme, icons, data).expect("mock app creation")
    }

    fn layout_80_cols() -> PanelLayout {
        let stream = Rect::new(41, 0, 39, 23);
        PanelLayout {
            workspaces: Rect::new(0, 0, 40, 20),
            keybinds: Rect::new(0, 20, 40, 3),
            stream_inner_height: stream.height.saturating_sub(2) as usize,
            stream,
            tab_bar: Rect::default(),
            tab_count: 0,
            detail: None,
            detail_height: 0,
            workspace_hits: Vec::new(),
            divider_col: 40,
            total_width: 80,
        }
    }

    #[test]
    fn click_on_divider_starts_drag() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            40,
            5,
            &layout,
            23,
        );
        assert!(app.dragging_divider);
    }

    #[test]
    fn click_adjacent_to_divider_starts_drag() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            41,
            5,
            &layout,
            23,
        );
        assert!(app.dragging_divider);
    }

    #[test]
    fn click_away_from_divider_does_not_drag() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            60,
            5,
            &layout,
            23,
        );
        assert!(!app.dragging_divider);
    }

    #[test]
    fn drag_updates_sidebar_pct() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            40,
            5,
            &layout,
            23,
        );
        assert!(app.dragging_divider);

        handle_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            60,
            5,
            &layout,
            23,
        );
        assert_eq!(app.sidebar_pct, 75);
        assert!(app.dragging_divider);
    }

    #[test]
    fn drag_clamps_to_min_max() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            40,
            5,
            &layout,
            23,
        );

        handle_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            5,
            &layout,
            23,
        );
        assert_eq!(app.sidebar_pct, MIN_SIDEBAR_PCT);

        handle_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            79,
            5,
            &layout,
            23,
        );
        assert_eq!(app.sidebar_pct, MAX_SIDEBAR_PCT);
    }

    #[test]
    fn mouse_up_ends_drag() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            40,
            5,
            &layout,
            23,
        );
        assert!(app.dragging_divider);

        handle_mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            50,
            5,
            &layout,
            23,
        );
        assert!(!app.dragging_divider);
    }

    #[test]
    fn stale_drag_cleared_by_new_click() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        app.dragging_divider = true;

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            60,
            5,
            &layout,
            23,
        );
        assert!(!app.dragging_divider);
    }
}
