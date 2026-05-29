// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for monitoring sessions and tailing events.
//!
//! Renders a unified chronological message stream with per-session hex
//! badges, scrolling, and a scrollbar.

pub mod app;
pub mod data;
pub mod filter;
pub mod format;
pub mod hints;
pub mod icons;
pub mod scope;
pub mod scrollbar;
pub mod sidebar;
pub mod stream;
pub mod theme;

pub use app::App;
pub use data::{DataSource, MockDataSource};

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
use ratatui::layout::Rect;
use tracing::info;

use crate::config::IconConfig;

use self::app::FocusRegion;
use self::data::SqliteDataSource;
use self::hints::render_hints;
use self::icons::IconSet;
use self::sidebar::render_sidebar;
use self::stream::render_stream;
use self::theme::Theme;

/// Minimum terminal width before the sidebar auto-hides.
const SIDEBAR_AUTO_HIDE_WIDTH: u16 = 60;

/// Tick interval for the event loop.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

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

/// Run the interactive TUI with a provided data source (test entry point).
///
/// Tests can inject a [`MockDataSource`]. No WAL watcher — falls back to
/// tick-based polling.
///
/// # Errors
///
/// Returns an error if terminal setup fails or session data cannot be read.
pub fn run_with_data(icon_config: IconConfig, data: Box<dyn DataSource>) -> Result<()> {
    run_with_data_and_watcher(icon_config, data, None)
}

/// Run the interactive TUI with an optional WAL watcher.
fn run_with_data_and_watcher(
    icon_config: IconConfig,
    data: Box<dyn DataSource>,
    wal_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let theme = Theme::detect();
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

/// Handle a key event, dispatching to global or focus-specific handlers.
fn handle_key(app: &mut App<'_>, code: KeyCode, show_sidebar: bool, viewport_height: usize) {
    // Global keys (always active regardless of focus).
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('b') => app.toggle_sidebar(),
        KeyCode::Tab => {
            if show_sidebar {
                app.cycle_focus();
            }
        }
        KeyCode::BackTab => {
            if show_sidebar {
                app.cycle_focus_back();
            }
        }
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

/// Handle a mouse event, dispatching to sidebar or stream based on position.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
fn handle_mouse(
    app: &mut App<'_>,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    sidebar_width: u16,
    show_sidebar: bool,
    stream_height: usize,
) {
    let in_sidebar = show_sidebar && column < sidebar_width;

    match kind {
        MouseEventKind::ScrollUp => {
            if in_sidebar {
                // Scroll whichever sidebar section is focused.
                match app.focus {
                    FocusRegion::Sessions => app.sidebar.cursor_up(3, stream_height),
                    FocusRegion::Servers => app.sidebar.server_cursor_up(3, stream_height),
                    FocusRegion::Stream => {}
                }
            } else {
                app.stream.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_sidebar {
                match app.focus {
                    FocusRegion::Sessions => app.sidebar.cursor_down(3, stream_height),
                    FocusRegion::Servers => app.sidebar.server_cursor_down(3, stream_height),
                    FocusRegion::Stream => {}
                }
            } else {
                app.stream.scroll_down(3, stream_height);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if in_sidebar {
                handle_sidebar_click(app, row as usize);
            } else {
                // Click in stream: move cursor, expand if scope header.
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

/// Handle a mouse click in the sidebar area.
///
/// Determines whether the click hit a session entry or a server entry
/// based on the row position, then toggles the corresponding filter.
fn handle_sidebar_click(app: &mut App<'_>, row: usize) {
    // Sessions header is row 0, entries start at row 1.
    let session_count = app.sidebar.entries.len();
    let session_end = 1 + session_count; // header + entries

    if row >= 1 && row < session_end {
        // Clicked a session entry.
        let entry_idx = row - 1;
        if entry_idx < app.sidebar.entries.len() {
            app.sidebar.cursor = entry_idx;
            app.toggle_session_selection();
        }
        return;
    }

    // Servers section: blank line + header + entries.
    if app.sidebar.servers.is_empty() {
        return;
    }
    let servers_header_row = session_end + 1; // blank + header
    if row <= servers_header_row {
        return;
    }

    // Walk server entries (each has 1 header + child_count children).
    let mut current_row = servers_header_row + 1;
    for (si, server) in app.sidebar.servers.iter().enumerate() {
        if row == current_row {
            // Clicked this server's header.
            app.sidebar.server_cursor = si;
            app.toggle_server_selection();
            return;
        }
        current_row += 1 + server.child_count();
    }
}

/// Main event loop — renders sidebar + message stream, handles input.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "render loop with sidebar + stream + hints + empty state in one draw closure; terminal widths are always small"
)]
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
    wal_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut last_sidebar_width: u16 = 0;

    loop {
        let size = terminal.size()?;
        let sidebar_fits = size.width >= SIDEBAR_AUTO_HIDE_WIDTH;
        let show_sidebar = app.sidebar_visible && sidebar_fits;
        // Move focus to stream when sidebar is not visible.
        if !show_sidebar && app.focus != FocusRegion::Stream {
            app.focus = FocusRegion::Stream;
        }
        // Reserve bottom row for hints bar.
        let content_height = size.height.saturating_sub(1);
        let stream_height = content_height as usize;
        app.stream.apply_auto_scroll(stream_height);

        terminal.draw(|f| {
            let area = f.area();
            let content_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: content_height,
            };
            let hints_area = Rect {
                x: area.x,
                y: area.y + content_height,
                width: area.width,
                height: 1.min(area.height),
            };

            // Empty state: no sessions and no messages.
            if app.sidebar.entries.is_empty()
                && app.sidebar.servers.is_empty()
                && app.stream.entries.is_empty()
            {
                let msg = "Waiting for connections\u{2026}";
                let msg_width = unicode_width::UnicodeWidthStr::width(msg) as u16;
                let x = content_area.x + content_area.width.saturating_sub(msg_width) / 2;
                let y = content_area.y + content_area.height / 2;
                f.buffer_mut().set_string(x, y, msg, app.theme.muted);
            } else if show_sidebar {
                let sidebar_width = app.sidebar.content_width().min(content_area.width / 2);
                last_sidebar_width = sidebar_width;
                let sidebar_area = Rect {
                    x: content_area.x,
                    y: content_area.y,
                    width: sidebar_width,
                    height: content_area.height,
                };
                let stream_area = Rect {
                    x: content_area.x + sidebar_width,
                    y: content_area.y,
                    width: content_area.width.saturating_sub(sidebar_width),
                    height: content_area.height,
                };
                render_sidebar(
                    &app.sidebar,
                    sidebar_area,
                    f.buffer_mut(),
                    app.theme,
                    app.focus,
                );
                render_stream(
                    &app.stream,
                    stream_area,
                    f.buffer_mut(),
                    app.theme,
                    app.icons,
                );
            } else {
                last_sidebar_width = 0;
                render_stream(
                    &app.stream,
                    content_area,
                    f.buffer_mut(),
                    app.theme,
                    app.icons,
                );
            }

            render_hints(hints_area, f.buffer_mut(), app.theme, app.focus);
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
                    handle_key(app, key.code, show_sidebar, stream_height);
                    app.fetch_page_if_needed();
                }
                Event::Mouse(mouse) => {
                    handle_mouse(
                        app,
                        mouse.kind,
                        mouse.column,
                        mouse.row,
                        last_sidebar_width,
                        show_sidebar,
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
