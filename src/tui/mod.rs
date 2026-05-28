// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for monitoring sessions and tailing events.
//!
//! Renders a unified chronological message stream with per-session hex
//! badges, scrolling, severity toggle, and a scrollbar.

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
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode};
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
        KeyCode::Char('d') => {
            app.level_threshold.toggle();
            let _ = app.reload_messages();
        }
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

/// Main event loop — renders sidebar + message stream, handles input.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
    wal_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let mut last_tick = Instant::now();

    loop {
        let size = terminal.size()?;
        let show_sidebar = size.width >= SIDEBAR_AUTO_HIDE_WIDTH;
        let stream_height = size.height as usize;
        app.stream.apply_auto_scroll(stream_height);

        terminal.draw(|f| {
            let area = f.area();

            if show_sidebar {
                let sidebar_width = app.sidebar.content_width().min(area.width / 2);
                let sidebar_area = Rect {
                    x: area.x,
                    y: area.y,
                    width: sidebar_width,
                    height: area.height,
                };
                let stream_area = Rect {
                    x: area.x + sidebar_width,
                    y: area.y,
                    width: area.width.saturating_sub(sidebar_width),
                    height: area.height,
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
                render_stream(&app.stream, area, f.buffer_mut(), app.theme, app.icons);
            }
        })?;

        if app.quit {
            return Ok(());
        }

        let timeout = TICK_INTERVAL
            .checked_sub(last_tick.elapsed())
            .unwrap_or_default();

        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
        {
            handle_key(app, key.code, show_sidebar, stream_height);
            app.fetch_page_if_needed();
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
