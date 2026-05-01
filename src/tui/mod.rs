// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for browsing sessions and tailing events.
//!
//! Two-pane layout with focus tracking:
//! - **Left pane**: sessions tree with workspace grouping, active/dead
//!   indicators, and navigation.
//! - **Right pane**: multi-panel events grid with BSP layout, scrollbars,
//!   filtering, visual selection, and mouse support.
//!
//! All colors use the terminal's ANSI palette so the TUI inherits whatever
//! theme the user has configured.

pub mod app;
pub mod category;
pub mod data;
pub mod degradation;
pub mod filter;
pub mod flat;
pub mod format;
pub mod grid;
pub mod hints;
pub mod icons;
pub mod layout;
pub mod mouse;
pub mod panel;
pub mod pipeline;
pub mod render;
pub mod scrollbar;
pub mod selection;
pub mod theme;
pub mod tree;

pub use app::App;
pub use data::{DataSource, MockDataSource};

use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::Watcher;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tracing::info;

use crate::config::IconConfig;
use crate::session;

use self::app::{FocusedPane, InputMode};
use self::data::SqliteDataSource;
use self::icons::IconSet;
use self::mouse::{DragState, MouseAction};
use self::render::{draw, handle_key_filter, handle_key_normal, handle_key_visual};
use self::theme::Theme;

/// Tick interval for the event loop.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// How often to check PID liveness (in ticks).
const LIVENESS_CHECK_TICKS: u64 = 150; // 150 * 200ms = 30s

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
    // Theme and icons live on the stack.
    let theme = Theme::detect();
    let icons = IconSet::from_config(icon_config);

    // Load TUI config for sessions width.
    let tui_config = crate::config::Config::load()
        .ok()
        .and_then(|c| c.tui)
        .unwrap_or_default();

    let mut app = App::new(
        &theme,
        &icons,
        data,
        tui_config.sessions_width,
        tui_config.keep_dead_panels,
    )?;

    // Load messages for auto-opened panels and create tails.
    let panel_ids: Vec<String> = app
        .grid
        .panels
        .iter()
        .map(|p| p.session_id.clone())
        .collect();
    let include_debug = app.level_threshold.include_debug();
    for id in &panel_ids {
        if let Ok(messages) = app.data.monitor_messages(id, include_debug)
            && let Some(panel) = app.grid.panels.iter_mut().find(|p| p.session_id == *id)
        {
            panel.load_messages(messages);
            panel.update_language_servers();
        }
        if let Ok(tail) = app.data.create_message_tail(id, include_debug) {
            app.tails.insert(id.clone(), tail);
        }
    }

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

/// Main event loop.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
    wal_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut tick_count = 0u64;

    loop {
        // Draw.
        terminal.draw(|f| draw(f, app))?;

        if app.quit {
            return Ok(());
        }

        // Poll for events.
        let timeout = TICK_INTERVAL
            .checked_sub(last_tick.elapsed())
            .unwrap_or_default();

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    let consumed = match app.input_mode {
                        InputMode::Normal => handle_key_normal(app, key),
                        InputMode::FilterInput => handle_key_filter(app, key),
                        InputMode::Visual => handle_key_visual(app, key),
                    };
                    let _ = consumed;
                }
                Event::Mouse(mouse) => {
                    handle_mouse(app, mouse);
                }
                _ => {}
            }
        }

        // Check for DB changes (non-blocking drain).
        let mut db_changed = false;
        if let Some(rx) = wal_rx {
            while rx.try_recv().is_ok() {
                db_changed = true;
            }
        }

        if db_changed {
            poll_tails(app);
            check_new_sessions(app);
        }

        // Tick.
        if last_tick.elapsed() >= TICK_INTERVAL {
            last_tick = Instant::now();
            tick_count += 1;

            // Fallback: poll on tick if no watcher (test mode).
            if wal_rx.is_none() {
                poll_tails(app);
            }

            // PID liveness check (catches crashes).
            if tick_count.is_multiple_of(LIVENESS_CHECK_TICKS) {
                check_session_liveness(app);
            }
        }
    }
}

/// Handle mouse events by resolving actions and dispatching.
#[allow(
    clippy::cast_possible_wrap,
    reason = "mouse scroll deltas are small integers"
)]
fn handle_mouse(app: &mut App<'_>, mouse: event::MouseEvent) {
    let layout = app.grid_layout.as_ref();

    match mouse.kind {
        MouseEventKind::Down(event::MouseButton::Left) => {
            let Some(layout) = layout else { return };
            let panel_scroll_offsets: Vec<usize> =
                app.grid.panels.iter().map(|p| p.scroll_offset).collect();
            let overflow_counts: Vec<scrollbar::OverflowCounts> = app
                .grid
                .panels
                .iter()
                .map(|p| {
                    let flat_len = p.flat_lines().len();
                    let viewport = if p.viewport_height > 0 {
                        p.viewport_height
                    } else {
                        20
                    };
                    let metrics = scrollbar::ScrollMetrics {
                        content_length: flat_len,
                        viewport_length: viewport,
                        position: p.scroll_offset,
                    };
                    scrollbar::compute_overflow(&metrics)
                })
                .collect();

            let border_x = app.tree_area.x + app.tree_area.width;
            let action = mouse::resolve_click(
                mouse.column,
                mouse.row,
                app.tree_area,
                layout,
                border_x,
                app.tree.scroll_offset,
                &panel_scroll_offsets,
                &overflow_counts,
            );

            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::ScrollDown => {
            let Some(layout) = layout else { return };
            let action = mouse::resolve_scroll(mouse.column, mouse.row, 3, app.tree_area, layout);
            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::ScrollUp => {
            let Some(layout) = layout else { return };
            let action = mouse::resolve_scroll(mouse.column, mouse.row, -3, app.tree_area, layout);
            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::Drag(event::MouseButton::Left) => {
            let Some(layout) = layout else { return };
            let panel_scroll_offsets: Vec<usize> =
                app.grid.panels.iter().map(|p| p.scroll_offset).collect();
            let action = mouse::resolve_drag(
                mouse.column,
                mouse.row,
                &app.drag_state,
                layout,
                &panel_scroll_offsets,
            );
            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::Up(event::MouseButton::Left) => {
            let action = mouse::resolve_release(&app.drag_state);
            dispatch_mouse_action(app, &action);
            app.drag_state = DragState::Idle;
        }
        _ => {}
    }
}

/// Dispatch a resolved mouse action.
#[allow(
    clippy::too_many_lines,
    reason = "match arms for each mouse action variant"
)]
fn dispatch_mouse_action(app: &mut App<'_>, action: &MouseAction) {
    match *action {
        MouseAction::FocusPanel(idx) => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(idx);
        }
        MouseAction::FocusTree => {
            app.focus = FocusedPane::Sessions;
        }
        MouseAction::SelectSession { item } => {
            app.focus = FocusedPane::Sessions;
            app.tree.cursor = item;
        }
        MouseAction::ToggleExpansion { panel, line } => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(panel);
            if let Some(p) = app.grid.panels.get_mut(panel) {
                p.cursor = line;
                p.toggle_expansion();
            }
        }
        MouseAction::TogglePin(idx) => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(idx);
            app.grid.toggle_pin();
        }
        MouseAction::ScrollPanel { panel, delta } => {
            // Mouse scroll moves the viewport only, not the cursor or focus.
            if let Some(p) = app.grid.panels.get_mut(panel) {
                p.scroll_viewport(delta);
            }
        }
        MouseAction::ScrollTree(delta) => {
            app.tree.navigate(delta);
        }
        MouseAction::StartBorderDrag { x } => {
            app.drag_state = DragState::BorderResize { initial_x: x };
        }
        MouseAction::ContinueBorderDrag { x } => {
            let terminal_width = app.tree_area.width + app.grid_area.width;
            if let Some(new_width) = mouse::compute_sessions_width_from_drag(x, terminal_width, 20)
            {
                app.sessions_width_ratio = f64::from(new_width) / f64::from(terminal_width);
            } else {
                app.sessions_visible = false;
            }
        }
        MouseAction::EndBorderDrag => {
            app.drag_state = DragState::Idle;
        }
        MouseAction::StartDragSelect { panel, line } => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(panel);
            if let Some(p) = app.grid.panels.get_mut(panel) {
                p.visual_selection = Some(selection::VisualSelection::new(line));
            }
            app.drag_state = DragState::LineSelect {
                panel,
                anchor: line,
            };
            app.input_mode = InputMode::Visual;
        }
        MouseAction::ContinueDragSelect { panel, line } => {
            if let Some(p) = app.grid.panels.get_mut(panel)
                && let Some(ref mut sel) = p.visual_selection
            {
                sel.extend(line);
            }
        }
        MouseAction::StartScrollbarDrag { panel, y } => {
            app.drag_state = DragState::Scrollbar { panel };
            scrollbar_click(app, panel, y);
        }
        MouseAction::ContinueScrollbarDrag { panel, y } => {
            scrollbar_click(app, panel, y);
        }
        MouseAction::JumpOverflow { panel, top } => {
            if let Some(p) = app.grid.panels.get_mut(panel) {
                if top {
                    p.scroll_offset = 0;
                } else {
                    let total = p.flat_lines().len();
                    let viewport = if p.viewport_height > 0 {
                        p.viewport_height
                    } else {
                        20
                    };
                    p.scroll_offset = total.saturating_sub(viewport);
                }
                p.tail_attached = false;
            }
        }
        MouseAction::None => {}
    }
}

/// Handle a scrollbar click/drag at `y` for the given panel.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
fn scrollbar_click(app: &mut App<'_>, panel: usize, y: u16) {
    if let Some(layout) = &app.grid_layout
        && let Some(pr) = layout.panels.get(panel)
    {
        let track = ratatui::layout::Rect::new(
            pr.rect.x + pr.rect.width.saturating_sub(1),
            pr.rect.y + 1,
            1,
            pr.rect.height.saturating_sub(1),
        );
        if let Some(p) = app.grid.panels.get_mut(panel) {
            let metrics = scrollbar::ScrollMetrics {
                content_length: p.flat_lines().len(),
                viewport_length: track.height as usize,
                position: p.scroll_offset,
            };
            let pos = scrollbar::scroll_position_from_click(y, track, &metrics);
            p.scroll_offset = pos;
            p.tail_attached = false;
        }
    }
}

/// Poll all message tails and push new messages into their panels.
fn poll_tails(app: &mut App<'_>) {
    let ids: Vec<String> = app.tails.keys().cloned().collect();

    for id in ids {
        let Some(tail) = app.tails.get_mut(&id) else {
            continue;
        };
        while let Ok(Some(msg)) = tail.try_next_message() {
            if let Some(panel) = app.grid.panels.iter_mut().find(|p| p.session_id == id) {
                panel.push_message(msg);
                panel.update_language_servers();
            }
        }
    }
}

/// Check for new sessions by comparing alive IDs against the tree.
///
/// Uses the lightweight `list_alive_session_ids` query. Only calls the
/// expensive `list_sessions` when new IDs are detected.
fn check_new_sessions(app: &mut App<'_>) {
    let Ok(alive_ids) = app.data.list_alive_session_ids() else {
        return;
    };

    // Collect known session IDs from the tree.
    let known_ids: Vec<&str> = app
        .tree
        .workspaces
        .iter()
        .flat_map(|ws| &ws.sessions)
        .map(|s| s.info.id.as_str())
        .collect();

    let has_new = alive_ids.iter().any(|id| !known_ids.contains(&id.as_str()));
    if !has_new {
        return;
    }

    // New sessions detected — full rebuild.
    let Ok(rows) = app.data.list_sessions() else {
        return;
    };

    // Preserve cursor position by session ID.
    let cursor_session_id = app.tree.selected_session_id().map(String::from);

    app.tree = tree::SessionTree::from_sessions(rows);

    // Restore cursor position.
    if let Some(ref id) = cursor_session_id {
        for (i, item) in app.tree.visible_items().iter().enumerate() {
            if let tree::TreeItem::Session { row, .. } = item
                && row.info.id == *id
            {
                app.tree.cursor = i;
                break;
            }
        }
    }

    // Panel display_id is the Catenary internal ID (set at construction).
    // No override needed — the internal ID is the unique per-panel identifier.

    // Auto-open panels for new alive sessions.
    let include_debug = app.level_threshold.include_debug();
    for id in &alive_ids {
        if app.grid.panel_for_session(id).is_none() {
            let idx = app.grid.open_panel(id.clone());
            if let Ok(messages) = app.data.monitor_messages(id, include_debug)
                && let Some(panel) = app.grid.panels.get_mut(idx)
            {
                panel.load_messages(messages);
                panel.update_language_servers();
            }
            if let Ok(tail) = app.data.create_message_tail(id, include_debug) {
                app.tails.insert(id.clone(), tail);
            }
        }
    }
}

/// Check PID liveness for all alive sessions in the tree.
///
/// No DB access — pure PID check via `session::is_process_alive`.
fn check_session_liveness(app: &mut App<'_>) {
    let alive: Vec<(String, u32)> = app
        .tree
        .alive_session_pids()
        .into_iter()
        .map(|(id, pid)| (id.to_string(), pid))
        .collect();

    let mut any_died = false;
    for (id, pid) in &alive {
        if !session::is_process_alive(*pid) {
            app.tree.mark_session_dead(id);
            if let Some(idx) = app.grid.panel_for_session(id) {
                if app.keep_dead_panels {
                    app.grid.panels[idx].alive = false;
                } else {
                    app.grid.close_panel(idx);
                    any_died = true;
                }
            }
            app.tails.remove(id);
        }
    }
    if any_died && app.grid.panels.is_empty() && app.focus == FocusedPane::Events {
        app.focus = FocusedPane::Sessions;
    }
}
