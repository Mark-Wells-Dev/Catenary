// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for monitoring sessions and tailing events.
//!
//! Renders a quadrant layout: Sessions (upper-left), Servers (lower-left),
//! a collapsible Keybinds panel, and a full-height message stream (right).

pub mod app;
pub mod data;
pub mod format;
pub mod hints;
pub mod icons;
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
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::Watcher;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Tabs, Widget};
use tracing::info;

use crate::config::IconConfig;

use self::app::{EffectiveLayout, FocusRegion};
use self::data::SqliteDataSource;
use self::hints::{KEYBINDS_EXPANDED_HEIGHT, render_keybinds_content};
use self::icons::IconSet;
use self::sidebar::{render_servers, render_sessions};
use self::stream::render_stream;
use self::theme::Theme;

/// Tick interval for the event loop.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Minimum sidebar width as a percentage of the terminal.
const MIN_SIDEBAR_PCT: u16 = 10;

/// Maximum sidebar width as a percentage of the terminal.
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
const LEFT_TAB_NAMES: &[&str] = &["Sessions", "Servers", "Keybinds"];
/// Tab label names for the full-width stack (includes Messages).
const FULL_TAB_NAMES: &[&str] = &["Sessions", "Servers", "Keybinds", "Messages"];

/// Stored panel rectangles and hit maps for mouse dispatch.
struct PanelLayout {
    sessions: Rect,
    servers: Rect,
    keybinds: Rect,
    stream: Rect,
    /// Tab bar area (empty rect when in Quadrant mode).
    tab_bar: Rect,
    /// Number of tabs rendered in the tab bar.
    tab_count: usize,
    /// Terminal row → session entry index.
    session_hits: Vec<(u16, usize)>,
    /// Terminal row → server entry index.
    server_hits: Vec<(u16, usize)>,
    /// Column at the right edge of the left panel (divider hit target).
    divider_col: u16,
    /// Total terminal width for percentage computation during drag.
    total_width: u16,
}

/// Handle a key event, dispatching to global or focus-specific handlers.
fn handle_key(app: &mut App<'_>, code: KeyCode, viewport_height: usize) {
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
        KeyCode::Tab => app.cycle_focus(),
        KeyCode::BackTab => app.cycle_focus_back(),
        _ => match app.focus {
            FocusRegion::Sessions => {
                let visible = viewport_height.saturating_sub(1);
                match code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.sidebar.cursor_down(1, visible);
                    }
                    KeyCode::Char('k') | KeyCode::Up => app.sidebar.cursor_up(1, visible),
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        app.toggle_session_selection();
                    }
                    _ => {}
                }
            }
            FocusRegion::Servers => {
                let visible = viewport_height.saturating_sub(1);
                match code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.sidebar.server_cursor_down(1, visible);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.sidebar.server_cursor_up(1, visible);
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        app.toggle_server_selection();
                    }
                    _ => {}
                }
            }
            FocusRegion::Keybinds => {
                // No navigation inside keybinds panel.
            }
            FocusRegion::Stream => {
                handle_stream_key(app, code, viewport_height);
            }
        },
    }
}

/// Handle a key event when the stream is focused.
fn handle_stream_key(app: &mut App<'_>, code: KeyCode, viewport_height: usize) {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.stream.cursor_down(1, viewport_height);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.stream.cursor_up(1);
        }
        KeyCode::Enter => {
            app.stream.toggle_expansion();
        }
        KeyCode::Char('y') => {
            if let Some(text) = app.stream.yank_text(app.icons) {
                osc52_copy(&text);
            }
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
        KeyCode::Esc => {
            app.stream.clear_search();
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
            // Confirm search and exit input mode.
            app.search_active = false;
            if app.search_input.is_empty() {
                app.stream.clear_search();
            }
        }
        KeyCode::Esc => {
            // Cancel search entirely.
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
///
/// Works through SSH, tmux, and modern terminals that support this
/// escape sequence. Falls back to a no-op if the write fails.
fn osc52_copy(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::style::Print(format!("\x1b]52;c;{encoded}\x07"))
    );
}

/// Minimal base64 encoder (RFC 4648). No external dependency needed.
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
    reason = "terminal coordinates are always small"
)]
fn handle_mouse(
    app: &mut App<'_>,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    layout: &PanelLayout,
    viewport_height: usize,
) {
    // ── Divider drag ───────────────────────────────────────────
    // Any left-button-down clears stale drag state; hits on the divider
    // boundary start a new drag and return early.
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
    let in_sessions = layout.sessions.contains(pos.into());
    let in_servers = layout.servers.contains(pos.into());
    let in_keybinds = layout.keybinds.contains(pos.into());
    let in_stream = layout.stream.contains(pos.into());

    match kind {
        MouseEventKind::ScrollUp => {
            if in_sessions {
                app.sidebar.cursor_up(3, viewport_height);
            } else if in_servers {
                app.sidebar.server_cursor_up(3, viewport_height);
            } else if in_stream {
                app.stream.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_sessions {
                app.sidebar.cursor_down(3, viewport_height);
            } else if in_servers {
                app.sidebar.server_cursor_down(3, viewport_height);
            } else if in_stream {
                app.stream.scroll_down(3, viewport_height);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if in_tab_bar && layout.tab_count > 0 {
                // Proportional tab hit detection.
                let relative_x = column.saturating_sub(layout.tab_bar.x);
                let clicked =
                    (relative_x as usize * layout.tab_count) / layout.tab_bar.width.max(1) as usize;
                let clicked = clicked.min(layout.tab_count - 1);
                app.set_left_tab(clicked);
            } else if in_sessions {
                app.focus = FocusRegion::Sessions;
                if let Some(&(_, idx)) = layout.session_hits.iter().find(|(r, _)| *r == row) {
                    app.sidebar.cursor = idx;
                    app.toggle_session_selection();
                }
            } else if in_servers {
                app.focus = FocusRegion::Servers;
                if let Some(&(_, idx)) = layout.server_hits.iter().find(|(r, _)| *r == row) {
                    app.sidebar.server_cursor = idx;
                    app.toggle_server_selection();
                }
            } else if in_keybinds {
                // In quadrant mode, clicking collapsed keybinds expands it.
                if app.effective == EffectiveLayout::Quadrant && !app.keybinds_expanded {
                    app.keybinds_expanded = true;
                }
                app.focus = FocusRegion::Keybinds;
            } else if in_stream {
                app.focus = FocusRegion::Stream;
                let stream_row = app.stream.scroll_position + row as usize;
                if stream_row < app.stream.display_rows.len() {
                    app.stream.cursor = stream_row;
                    app.stream.auto_scroll = false;
                    app.stream.toggle_expansion();
                }
            }
        }
        _ => {}
    }
}

/// Build a `Block` frame for a panel, styling the border based on focus.
fn panel_block<'a>(title: &'a str, focused: bool, theme: &'a Theme) -> Block<'a> {
    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let title_style = if focused { theme.title } else { theme.muted };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title, title_style))
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
        // Right-align the status indicator.
        let prefix_len = 1 + input.len();
        let status_len = status.len() + 1; // space + status
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

/// Render Sessions into `panel_rect`, returning the session hit map.
fn render_sessions_panel(
    app: &App<'_>,
    panel_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
) {
    let focused = app.focus == FocusRegion::Sessions;
    let block = panel_block(" Sessions ", focused, app.theme);
    let inner = block.inner(panel_rect);
    block.render(panel_rect, buf);
    layout.session_hits = render_sessions(&app.sidebar, inner, buf, app.theme, focused);
    layout.sessions = panel_rect;
}

/// Render Servers into `panel_rect`, returning the server hit map.
fn render_servers_panel(
    app: &App<'_>,
    panel_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
) {
    let focused = app.focus == FocusRegion::Servers;
    let block = panel_block(" Servers ", focused, app.theme);
    let inner = block.inner(panel_rect);
    block.render(panel_rect, buf);
    layout.server_hits = render_servers(&app.sidebar, inner, buf, app.theme, focused);
    layout.servers = panel_rect;
}

/// Render the Keybinds panel at full height (for stacked/full-stack mode).
fn render_keybinds_panel(
    app: &App<'_>,
    panel_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
) {
    let focused = app.focus == FocusRegion::Keybinds;
    let block = panel_block(" Keybinds ", focused, app.theme);
    let inner = block.inner(panel_rect);
    block.render(panel_rect, buf);
    render_keybinds_content(inner, buf, app.theme);
    layout.keybinds = panel_rect;
}

/// Render the Messages stream panel into `panel_rect`, including the search
/// bar when active. Returns an optional cursor position for the search input.
#[allow(
    clippy::cast_possible_truncation,
    reason = "search input length is always small"
)]
fn render_messages_panel(
    app: &App<'_>,
    panel_rect: Rect,
    buf: &mut Buffer,
    layout: &mut PanelLayout,
) -> Option<(u16, u16)> {
    let focused = app.focus == FocusRegion::Stream;
    let block = panel_block(" Messages ", focused, app.theme);
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
        sessions: Rect::default(),
        servers: Rect::default(),
        keybinds: Rect::default(),
        stream: Rect::default(),
        tab_bar: Rect::default(),
        tab_count: 0,
        session_hits: Vec::new(),
        server_hits: Vec::new(),
        divider_col: 0,
        total_width: 0,
    };

    loop {
        let size = terminal.size()?;
        let stream_height = size.height as usize;
        app.stream.recompute_search_if_dirty(app.icons);
        app.update_effective(size.width, size.height);
        app.stream.apply_auto_scroll(stream_height);

        terminal.draw(|f| {
            let area = f.area();

            // Empty state: nothing to show the user.
            if app.sidebar.entries.is_empty()
                && app.sidebar.servers.is_empty()
                && app.sidebar.dead_servers.is_empty()
                && app.stream.entries.is_empty()
            {
                let msg = "Waiting for connections\u{2026}";
                let msg_width = unicode_width::UnicodeWidthStr::width(msg) as u16;
                let x = area.x + area.width.saturating_sub(msg_width) / 2;
                let y = area.y + area.height / 2;
                f.buffer_mut().set_string(x, y, msg, app.theme.muted);
                layout.session_hits.clear();
                layout.server_hits.clear();
                layout.sessions = Rect::default();
                layout.servers = Rect::default();
                layout.keybinds = Rect::default();
                layout.stream = Rect::default();
                layout.tab_bar = Rect::default();
                layout.tab_count = 0;
                layout.divider_col = 0;
                layout.total_width = 0;
                return;
            }

            // Reset per-frame mouse-dispatch state.
            layout.session_hits.clear();
            layout.server_hits.clear();
            layout.sessions = Rect::default();
            layout.servers = Rect::default();
            layout.keybinds = Rect::default();
            layout.stream = Rect::default();
            layout.tab_bar = Rect::default();
            layout.tab_count = 0;
            layout.divider_col = 0;
            layout.total_width = 0;

            // Cursor position for the search bar (set by render_messages_panel).
            let mut search_cursor: Option<(u16, u16)> = None;

            match app.effective {
                // ── Quadrant: three left panels + Messages right ────
                EffectiveLayout::Quadrant => {
                    let h_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(app.sidebar_pct),
                            Constraint::Percentage(100 - app.sidebar_pct),
                        ])
                        .split(area);

                    let left = h_chunks[0];
                    let right = h_chunks[1];

                    layout.divider_col = left.x + left.width.saturating_sub(1);
                    layout.total_width = area.width;

                    let keybinds_height = if app.keybinds_expanded {
                        KEYBINDS_EXPANDED_HEIGHT + 2
                    } else {
                        3
                    };

                    let v_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Fill(1),
                            Constraint::Fill(1),
                            Constraint::Length(keybinds_height),
                        ])
                        .split(left);

                    let sessions_rect = v_chunks[0];
                    let servers_rect = v_chunks[1];
                    let keybinds_rect = v_chunks[2];

                    // Sessions.
                    render_sessions_panel(app, sessions_rect, f.buffer_mut(), &mut layout);

                    // Servers.
                    render_servers_panel(app, servers_rect, f.buffer_mut(), &mut layout);

                    // Keybinds (collapsed/expanded).
                    let keybinds_focused = app.focus == FocusRegion::Keybinds;
                    let keybinds_title = if app.keybinds_expanded {
                        " Keybinds  ? "
                    } else {
                        " Keybinds  ? to expand "
                    };
                    let keybinds_block = panel_block(keybinds_title, keybinds_focused, app.theme);
                    let keybinds_inner = keybinds_block.inner(keybinds_rect);
                    keybinds_block.render(keybinds_rect, f.buffer_mut());
                    if app.keybinds_expanded {
                        render_keybinds_content(keybinds_inner, f.buffer_mut(), app.theme);
                    }
                    layout.keybinds = keybinds_rect;

                    // Messages.
                    search_cursor = render_messages_panel(app, right, f.buffer_mut(), &mut layout);
                }

                // ── Stacked: tab bar + active panel left, Messages right
                EffectiveLayout::Stacked => {
                    let h_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(app.sidebar_pct),
                            Constraint::Percentage(100 - app.sidebar_pct),
                        ])
                        .split(area);

                    let left = h_chunks[0];
                    let right = h_chunks[1];

                    layout.divider_col = left.x + left.width.saturating_sub(1);
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

                    match app.active_left_tab {
                        0 => render_sessions_panel(app, panel_rect, f.buffer_mut(), &mut layout),
                        1 => render_servers_panel(app, panel_rect, f.buffer_mut(), &mut layout),
                        _ => render_keybinds_panel(app, panel_rect, f.buffer_mut(), &mut layout),
                    }

                    // Messages (always visible in stacked mode).
                    search_cursor = render_messages_panel(app, right, f.buffer_mut(), &mut layout);
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
                        3
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
                        0 => render_sessions_panel(app, panel_rect, f.buffer_mut(), &mut layout),
                        1 => render_servers_panel(app, panel_rect, f.buffer_mut(), &mut layout),
                        2 => render_keybinds_panel(app, panel_rect, f.buffer_mut(), &mut layout),
                        _ => {
                            search_cursor =
                                render_messages_panel(app, panel_rect, f.buffer_mut(), &mut layout);
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
                    handle_key(app, key.code, stream_height);
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
            // Drain WAL notification channel (coalesce multiple signals).
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
        // Simulate an 80-column terminal with 50% sidebar (40 cols each).
        PanelLayout {
            sessions: Rect::new(0, 0, 40, 10),
            servers: Rect::new(0, 10, 40, 10),
            keybinds: Rect::new(0, 20, 40, 3),
            stream: Rect::new(40, 0, 40, 23),
            tab_bar: Rect::default(),
            tab_count: 0,
            session_hits: Vec::new(),
            server_hits: Vec::new(),
            divider_col: 39, // left.x + left.width - 1
            total_width: 80,
        }
    }

    #[test]
    fn click_on_divider_starts_drag() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        // Click exactly on divider_col.
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            39,
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

        // Click one column to the right of divider_col (stream border).
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
    fn click_away_from_divider_does_not_drag() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = make_app(&theme, &icons);
        let layout = layout_80_cols();

        // Click well inside the stream panel.
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

        // Start drag.
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            39,
            5,
            &layout,
            23,
        );
        assert!(app.dragging_divider);

        // Drag to column 60 on an 80-col terminal → 75%.
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

        // Start drag.
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            39,
            5,
            &layout,
            23,
        );

        // Drag to column 0 → clamped to MIN_SIDEBAR_PCT (10).
        handle_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            5,
            &layout,
            23,
        );
        assert_eq!(app.sidebar_pct, MIN_SIDEBAR_PCT);

        // Drag to column 79 → 98% → clamped to MAX_SIDEBAR_PCT (90).
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

        // Start drag.
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            39,
            5,
            &layout,
            23,
        );
        assert!(app.dragging_divider);

        // Release.
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

        // Force stale drag state (e.g., mouse released outside terminal).
        app.dragging_divider = true;

        // Click away from divider — should clear drag and proceed normally.
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
