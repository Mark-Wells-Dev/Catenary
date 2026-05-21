// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for monitoring sessions and tailing events.
//!
//! Renders a unified chronological message stream with per-session hex
//! badges, scrolling, severity toggle, and a scrollbar.

pub mod app;
pub mod category;
pub mod data;
pub mod filter;
pub mod format;
pub mod hints;
pub mod icons;
pub mod pipeline;
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
use tracing::info;

use crate::config::IconConfig;

use self::data::SqliteDataSource;
use self::icons::IconSet;
use self::stream::render_stream;
use self::theme::Theme;

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

/// Main event loop — renders unified message stream, handles input.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
    wal_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let mut last_tick = Instant::now();

    loop {
        let viewport_height = terminal.size()?.height as usize;
        app.stream.apply_auto_scroll(viewport_height);

        terminal.draw(|f| {
            let area = f.area();
            render_stream(&app.stream, area, f.buffer_mut(), app.theme, app.icons);
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
            match key.code {
                KeyCode::Char('q') => app.quit = true,
                KeyCode::Char('d') => {
                    app.level_threshold.toggle();
                    let _ = app.reload_messages();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    app.stream.scroll_down(1, viewport_height);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.stream.scroll_up(1);
                }
                KeyCode::PageDown => {
                    app.stream.scroll_down(viewport_height / 2, viewport_height);
                }
                KeyCode::PageUp => {
                    app.stream.scroll_up(viewport_height / 2);
                }
                KeyCode::Home => {
                    app.stream.scroll_position = 0;
                    app.stream.auto_scroll = false;
                }
                KeyCode::End => {
                    app.stream.pin_to_bottom(viewport_height);
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
            last_tick = Instant::now();
        }
    }
}
