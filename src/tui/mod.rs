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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Widget};
use tracing::info;

use crate::config::IconConfig;

use self::app::FocusRegion;
use self::data::SqliteDataSource;
use self::hints::{KEYBINDS_EXPANDED_HEIGHT, render_keybinds_content};
use self::icons::IconSet;
use self::sidebar::{render_servers, render_sessions};
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

/// Stored panel rectangles and hit maps for mouse dispatch.
struct PanelLayout {
    sessions: Rect,
    servers: Rect,
    keybinds: Rect,
    stream: Rect,
    /// Terminal row → session entry index.
    session_hits: Vec<(u16, usize)>,
    /// Terminal row → server entry index.
    server_hits: Vec<(u16, usize)>,
}

/// Handle a key event, dispatching to global or focus-specific handlers.
fn handle_key(app: &mut App<'_>, code: KeyCode, viewport_height: usize) {
    // Global keys (always active regardless of focus).
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') => app.toggle_keybinds(),
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
    let pos = (column, row);
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
            if in_sessions {
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
            } else if in_keybinds && app.keybinds_expanded {
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

/// Main event loop — renders quadrant layout, handles input.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "render loop with quadrant layout + empty state in one draw closure; terminal widths are always small"
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
        session_hits: Vec::new(),
        server_hits: Vec::new(),
    };

    loop {
        let size = terminal.size()?;
        let stream_height = size.height as usize;
        app.stream.apply_auto_scroll(stream_height);

        terminal.draw(|f| {
            let area = f.area();

            // Empty state: no sessions and no messages.
            if app.sidebar.entries.is_empty()
                && app.sidebar.servers.is_empty()
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
                return;
            }

            // ── Horizontal split: left (50%) | right (50%) ──────────
            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);

            let left = h_chunks[0];
            let right = h_chunks[1];

            // ── Left column: Sessions | Servers | Keybinds ──────────
            let keybinds_height = if app.keybinds_expanded {
                // +2 for Block border top/bottom.
                KEYBINDS_EXPANDED_HEIGHT + 2
            } else {
                // Collapsed: title bar only (top border + bottom border + title row = 3).
                // But with Block borders, a height of 3 shows the frame with 1 inner row.
                // We want just the frame border lines (no inner content) = height of 2
                // would show top + bottom border with no inner. But Borders::ALL needs
                // at least 2 rows for the top and bottom borders.
                // In practice 3 rows = top border + 1 line title area + bottom border.
                3
            };

            let v_constraints = vec![
                Constraint::Fill(1),                 // Sessions
                Constraint::Fill(1),                 // Servers
                Constraint::Length(keybinds_height), // Keybinds
            ];
            let v_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(v_constraints)
                .split(left);

            let sessions_rect = v_chunks[0];
            let servers_rect = v_chunks[1];
            let keybinds_rect = v_chunks[2];

            // ── Sessions panel ──────────────────────────────────────
            let sessions_focused = app.focus == FocusRegion::Sessions;
            let sessions_block = panel_block(" Sessions ", sessions_focused, app.theme);
            let sessions_inner = sessions_block.inner(sessions_rect);
            sessions_block.render(sessions_rect, f.buffer_mut());
            layout.session_hits = render_sessions(
                &app.sidebar,
                sessions_inner,
                f.buffer_mut(),
                app.theme,
                sessions_focused,
            );

            // ── Servers panel ───────────────────────────────────────
            let servers_focused = app.focus == FocusRegion::Servers;
            let servers_block = panel_block(" Servers ", servers_focused, app.theme);
            let servers_inner = servers_block.inner(servers_rect);
            servers_block.render(servers_rect, f.buffer_mut());
            layout.server_hits = render_servers(
                &app.sidebar,
                servers_inner,
                f.buffer_mut(),
                app.theme,
                servers_focused,
            );

            // ── Keybinds panel ──────────────────────────────────────
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

            // ── Stream panel ────────────────────────────────────────
            let stream_focused = app.focus == FocusRegion::Stream;
            let stream_block = panel_block(" Messages ", stream_focused, app.theme);
            let stream_inner = stream_block.inner(right);
            stream_block.render(right, f.buffer_mut());
            render_stream(
                &app.stream,
                stream_inner,
                f.buffer_mut(),
                app.theme,
                app.icons,
            );

            // Store rects for mouse dispatch (use full panel rects).
            layout.sessions = sessions_rect;
            layout.servers = servers_rect;
            layout.keybinds = keybinds_rect;
            layout.stream = right;
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
