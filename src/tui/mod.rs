// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive `state.json` dashboard.
//!
//! The TUI renders four boards from the daemon-owned snapshot — server health
//! (upper-left), sessions (lower-left), the activity ring of milestones
//! (upper-right), and the alerts ring (lower-right) — with a collapsible
//! keybinds panel. It is a **pure file reader**: it file-watches the
//! snapshot and re-loads on change. It never opens the firehose (JSONL) or a
//! database, which makes it structurally unwedgeable (observability ticket 06).
//! The bridge to `catenary query` is a yankable scope id (OSC 52).

pub mod app;
pub mod data;
pub mod format;
pub mod hints;
pub mod icons;
pub mod scrollbar;
pub mod theme;

pub use app::App;
pub use data::DataSource;

use std::io;
use std::path::{Path, PathBuf};
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
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use tracing::info;
use unicode_width::UnicodeWidthStr;

use crate::config::IconConfig;

use self::app::{Board, Focus};
use self::data::StateJsonDataSource;
use self::hints::{KEYBINDS_EXPANDED_HEIGHT, render_keybinds_content};
use self::icons::IconSet;
use self::scrollbar::{OverflowCounts, render_overflow_counts};
use self::theme::Theme;

/// Tick interval for the event loop. The snapshot is re-loaded each tick (a
/// small file read), so changes surface within this bound even without the
/// watcher.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Minimum left-column width percentage.
const MIN_SIDEBAR_PCT: u16 = 10;
/// Maximum left-column width percentage.
const MAX_SIDEBAR_PCT: u16 = 90;
/// Terminal width below which the layout degrades to three stacked full-width
/// boards (the right alerts pane folds under the left column).
const NARROW_THRESHOLD: u16 = 60;

/// Rendered lines per server board entry.
const SERVER_LPE: usize = format::SERVER_ENTRY_LINES;
/// Rendered lines per session board entry.
const SESSION_LPE: usize = format::SESSION_ENTRY_LINES;
/// Rendered lines per activity (milestone) entry.
const ACTIVITY_LPE: usize = 1;
/// Rendered lines per alert entry.
const ALERT_LPE: usize = 1;

/// Start a file watcher on the snapshot's parent directory.
///
/// Watches the parent (non-recursive) because `state.json` is replaced by an
/// atomic temp + rename, and may not exist when the TUI starts. Events are
/// filtered to the `state.json` filename and coalesced into a `()` signal.
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
    // The parent may not exist yet (daemon never started); create it so the
    // watch can attach and fire when the daemon first writes.
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

    let watcher = match start_state_watcher(&snapshot_path) {
        Ok((watcher, rx)) => Some((watcher, rx)),
        Err(e) => {
            info!("state.json watcher unavailable, falling back to polling: {e}");
            None
        }
    };
    // Hold `_watcher` to keep it alive; extract `rx` for the event loop.
    let (_watcher, watch_rx) = match watcher {
        Some((w, rx)) => (Some(w), Some(rx)),
        None => (None, None),
    };

    run_with_data_and_watcher(icon_config, Box::new(data), watch_rx.as_ref())
}

/// Run the dashboard with an explicit data source and optional change signal.
fn run_with_data_and_watcher(
    icon_config: IconConfig,
    data: Box<dyn DataSource>,
    watch_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let theme = Theme::new();
    let icons = IconSet::from_config(icon_config);

    let mut app = App::new(&theme, &icons, data)?;

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

/// Stored panel rectangles for mouse dispatch.
#[derive(Default)]
struct PanelLayout {
    servers: Rect,
    servers_inner: Rect,
    sessions: Rect,
    sessions_inner: Rect,
    activity: Rect,
    activity_inner: Rect,
    alerts: Rect,
    alerts_inner: Rect,
    keybinds: Rect,
    /// Column of the vertical divider (0 when there is none, e.g. narrow mode).
    divider_col: u16,
    /// Total terminal width, for percentage computation during a divider drag.
    total_width: u16,
}

/// Handle a key event.
fn handle_key(app: &mut App<'_>, code: KeyCode) {
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') => app.toggle_keybinds(),
        KeyCode::Tab => app.cycle_focus(),
        KeyCode::BackTab => app.cycle_focus_back(),
        KeyCode::Char('j') | KeyCode::Down => app.cursor_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.cursor_up(1),
        KeyCode::PageDown => app.page_down(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::Char('g') | KeyCode::Home => app.jump_home(),
        KeyCode::Char('G') | KeyCode::End => app.jump_end(),
        KeyCode::Char('y') => {
            if let Some(text) = app.selected_yank_text() {
                osc52_copy(&text);
            }
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

/// Map a terminal row inside a board's content area to an entry index.
fn entry_at(
    inner: Rect,
    row: u16,
    scroll: usize,
    lines_per_entry: usize,
    len: usize,
) -> Option<usize> {
    if row < inner.y || row >= inner.y + inner.height || lines_per_entry == 0 {
        return None;
    }
    let rel = (row - inner.y) as usize;
    let entry = scroll + rel / lines_per_entry;
    (entry < len).then_some(entry)
}

/// Handle a mouse event.
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
) {
    // ── Divider drag (wide layout only) ────────────────────────
    if matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
        app.dragging_divider = layout.divider_col > 0 && column.abs_diff(layout.divider_col) <= 1;
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

    let pos = (column, row);
    let target = if layout.servers.contains(pos.into()) {
        Some(Focus::Servers)
    } else if layout.sessions.contains(pos.into()) {
        Some(Focus::Sessions)
    } else if layout.activity.contains(pos.into()) {
        Some(Focus::Activity)
    } else if layout.alerts.contains(pos.into()) {
        Some(Focus::Alerts)
    } else {
        None
    };

    match kind {
        MouseEventKind::ScrollUp => {
            if let Some(focus) = target {
                app.focus = focus;
                app.cursor_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if let Some(focus) = target {
                app.focus = focus;
                app.cursor_down(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => match target {
            Some(Focus::Servers) => {
                app.focus = Focus::Servers;
                if let Some(i) = entry_at(
                    layout.servers_inner,
                    row,
                    app.servers.scroll,
                    SERVER_LPE,
                    app.snapshot.servers.len(),
                ) {
                    app.servers.cursor = i;
                }
            }
            Some(Focus::Sessions) => {
                app.focus = Focus::Sessions;
                if let Some(i) = entry_at(
                    layout.sessions_inner,
                    row,
                    app.sessions.scroll,
                    SESSION_LPE,
                    app.snapshot.sessions.len(),
                ) {
                    app.sessions.cursor = i;
                }
            }
            Some(Focus::Activity) => {
                app.focus = Focus::Activity;
                if let Some(i) = entry_at(
                    layout.activity_inner,
                    row,
                    app.activity.scroll,
                    ACTIVITY_LPE,
                    app.snapshot.activity.len(),
                ) {
                    app.activity.cursor = i;
                }
            }
            Some(Focus::Alerts) => {
                app.focus = Focus::Alerts;
                if let Some(i) = entry_at(
                    layout.alerts_inner,
                    row,
                    app.alerts.scroll,
                    ALERT_LPE,
                    app.snapshot.alerts.len(),
                ) {
                    app.alerts.cursor = i;
                }
            }
            None => {
                if layout.keybinds.contains(pos.into()) {
                    app.keybinds_expanded = true;
                }
            }
        },
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

/// Inner content rect for a panel with the given borders.
fn inner_of(rect: Rect, borders: Borders) -> Rect {
    Block::default().borders(borders).inner(rect)
}

/// Re-style a line for the selection highlight, padding to `width` so the whole
/// row (including a 2-line entry's sub-line) is highlighted.
fn highlight_line(line: &Line<'static>, width: usize, style: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|s| Span::styled(s.content.clone(), s.style.patch(style)))
        .collect();
    let used: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    Line::from(spans)
}

/// Render a board (entries are line-groups) into `rect`, with cursor highlight,
/// scroll, and overflow indicators.
#[allow(
    clippy::too_many_arguments,
    reason = "a board render needs frame, content, scroll state, and styling"
)]
fn render_board_into(
    title: &str,
    focused: bool,
    entries: &[Vec<Line<'static>>],
    lines_per_entry: usize,
    board: &mut Board,
    rect: Rect,
    inner: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    borders: Borders,
) {
    panel_block(title, focused, theme, borders).render(rect, buf);

    let total = entries.len();
    let visible = (inner.height as usize) / lines_per_entry.max(1);
    board.visible = visible;
    board.settle(total);

    let mut y = inner.y;
    let end_y = inner.y + inner.height;
    'outer: for (i, lines) in entries.iter().enumerate().skip(board.scroll).take(visible) {
        let selected = focused && i == board.cursor;
        for line in lines {
            if y >= end_y {
                break 'outer;
            }
            if selected {
                let hl = highlight_line(line, inner.width as usize, theme.selection);
                buf.set_line(inner.x, y, &hl, inner.width);
            } else {
                buf.set_line(inner.x, y, line, inner.width);
            }
            y += 1;
        }
    }

    let counts = OverflowCounts {
        above: board.scroll,
        below: total.saturating_sub(board.scroll + visible),
    };
    render_overflow_counts(&counts, inner, buf, theme.muted);
}

/// Render a horizontal separator row carrying a panel title.
///
/// `left_cap` anchors a `├` at the left border (full-box / left-column
/// separators); when false the left end stays `─` so the vertical divider draws
/// the junction instead (the right column's separator). `right_cap` adds a `┤`
/// at the right edge (full-box, narrow mode); in the wide left column the right
/// end abuts the divider, which draws that junction.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "terminal coordinates are always small; a titled separator needs position, content, style, and the two cap flags"
)]
fn render_separator(
    y: u16,
    x: u16,
    width: u16,
    title: &str,
    theme: &Theme,
    buf: &mut Buffer,
    left_cap: bool,
    right_cap: bool,
) {
    if width == 0 {
        return;
    }
    let style = theme.border_unfocused;
    for col in x..(x + width) {
        buf.set_string(col, y, "─", style);
    }
    if left_cap {
        buf.set_string(x, y, "├", style);
    }
    if right_cap {
        buf.set_string(x + width - 1, y, "┤", style);
    }
    if !title.is_empty() && width > 4 {
        let max = (width - 4) as usize;
        let truncated: String = title.chars().take(max).collect();
        buf.set_string(x + 2, y, &truncated, theme.muted);
    }
}

/// Render the vertical divider column, using intersection glyphs where the
/// left-column separators (`┤`) and the right-column separator (`├`) meet it.
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
        let left = left_seps.contains(&y);
        let right = right_seps.contains(&y);
        let ch = if y == top {
            "┬"
        } else if y == bottom {
            "┴"
        } else if left && right {
            "┼"
        } else if left {
            "┤"
        } else if right {
            "├"
        } else {
            "│"
        };
        buf.set_string(col, y, ch, style);
    }
}

/// The daemon status line shown in the alerts panel title.
fn daemon_status(snapshot: &crate::state_snapshot::Snapshot) -> String {
    if snapshot.daemon.generated_at.is_empty() {
        return " Alerts ".to_string();
    }
    let age = format::elapsed_short(&snapshot.daemon.generated_at);
    let version = if snapshot.daemon.version.is_empty() {
        "catenary"
    } else {
        &snapshot.daemon.version
    };
    if age.is_empty() {
        format!(" Alerts — {version} ")
    } else {
        format!(" Alerts — {version} · updated {age} ago ")
    }
}

/// Build per-entry line groups for the server board.
fn build_server_entries(
    app: &App<'_>,
    width: u16,
    theme: &Theme,
    icons: &IconSet,
) -> Vec<Vec<Line<'static>>> {
    app.snapshot
        .servers
        .iter()
        .map(|s| format::server_entry_lines(s, width as usize, theme, icons))
        .collect()
}

/// Build per-entry line groups for the session board.
fn build_session_entries(
    app: &App<'_>,
    width: u16,
    theme: &Theme,
    icons: &IconSet,
) -> Vec<Vec<Line<'static>>> {
    app.snapshot
        .sessions
        .iter()
        .map(|s| format::session_entry_lines(s, width as usize, theme, icons))
        .collect()
}

/// Build per-entry line groups for the alerts ring (one line per alert).
fn build_alert_entries(
    app: &App<'_>,
    width: u16,
    theme: &Theme,
    icons: &IconSet,
) -> Vec<Vec<Line<'static>>> {
    app.snapshot
        .alerts
        .iter()
        .map(|a| vec![format::alert_line(a, width as usize, theme, icons)])
        .collect()
}

/// Build per-entry line groups for the activity ring (one line per milestone).
fn build_activity_entries(
    app: &App<'_>,
    width: u16,
    theme: &Theme,
    icons: &IconSet,
) -> Vec<Vec<Line<'static>>> {
    app.snapshot
        .activity
        .iter()
        .map(|m| vec![format::milestone_line(m, width as usize, theme, icons)])
        .collect()
}

/// Render a single dashboard frame. Shared by the event loop and tests so both
/// exercise the same render path.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "one draw function covers the waiting state plus the wide and narrow layouts"
)]
fn render_dashboard(f: &mut Frame<'_>, app: &mut App<'_>, layout: &mut PanelLayout) {
    let area = f.area();
    *layout = PanelLayout::default();

    // ── Too small / waiting states ─────────────────────────────
    if area.width < 16 || area.height < 4 {
        return;
    }
    if !app.daemon_present() {
        let msg = "Waiting for daemon\u{2026}";
        let w = UnicodeWidthStr::width(msg) as u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height / 2;
        f.buffer_mut().set_string(x, y, msg, app.theme.muted);
        return;
    }

    let theme = app.theme;
    let icons = app.icons;
    let focus = app.focus;

    if area.width < NARROW_THRESHOLD {
        // ── Narrow: four stacked full-width boards ─────────────
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .split(area);
        let servers_rect = v[0];
        let sep_a = v[1];
        let sessions_rect = v[2];
        let sep_b = v[3];
        let activity_rect = v[4];
        let sep_c = v[5];
        let alerts_rect = v[6];

        let sb = Borders::TOP | Borders::LEFT | Borders::RIGHT;
        let mb = Borders::LEFT | Borders::RIGHT;
        let ab = Borders::LEFT | Borders::RIGHT | Borders::BOTTOM;
        let servers_inner = inner_of(servers_rect, sb);
        let sessions_inner = inner_of(sessions_rect, mb);
        let activity_inner = inner_of(activity_rect, mb);
        let alerts_inner = inner_of(alerts_rect, ab);

        let server_entries = build_server_entries(app, servers_inner.width, theme, icons);
        let session_entries = build_session_entries(app, sessions_inner.width, theme, icons);
        let activity_entries = build_activity_entries(app, activity_inner.width, theme, icons);
        let alert_entries = build_alert_entries(app, alerts_inner.width, theme, icons);

        render_board_into(
            " Servers ",
            focus == Focus::Servers,
            &server_entries,
            SERVER_LPE,
            &mut app.servers,
            servers_rect,
            servers_inner,
            f.buffer_mut(),
            theme,
            sb,
        );
        render_separator(
            sep_a.y,
            sep_a.x,
            sep_a.width,
            " Sessions ",
            theme,
            f.buffer_mut(),
            true,
            true,
        );
        render_board_into(
            "",
            focus == Focus::Sessions,
            &session_entries,
            SESSION_LPE,
            &mut app.sessions,
            sessions_rect,
            sessions_inner,
            f.buffer_mut(),
            theme,
            mb,
        );
        render_separator(
            sep_b.y,
            sep_b.x,
            sep_b.width,
            " Activity ",
            theme,
            f.buffer_mut(),
            true,
            true,
        );
        render_board_into(
            "",
            focus == Focus::Activity,
            &activity_entries,
            ACTIVITY_LPE,
            &mut app.activity,
            activity_rect,
            activity_inner,
            f.buffer_mut(),
            theme,
            mb,
        );
        render_separator(
            sep_c.y,
            sep_c.x,
            sep_c.width,
            " Alerts ",
            theme,
            f.buffer_mut(),
            true,
            true,
        );
        render_board_into(
            "",
            focus == Focus::Alerts,
            &alert_entries,
            ALERT_LPE,
            &mut app.alerts,
            alerts_rect,
            alerts_inner,
            f.buffer_mut(),
            theme,
            ab,
        );

        layout.servers = servers_rect;
        layout.servers_inner = servers_inner;
        layout.sessions = sessions_rect;
        layout.sessions_inner = sessions_inner;
        layout.activity = activity_rect;
        layout.activity_inner = activity_inner;
        layout.alerts = alerts_rect;
        layout.alerts_inner = alerts_inner;
        return;
    }

    // ── Wide: left column (servers/sessions/keybinds) + right
    //    column (activity over alerts) ─────────────────────────
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(app.sidebar_pct),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(area);
    let left = h[0];
    let divider = h[1];
    let right = h[2];

    let kb_height = if app.keybinds_expanded {
        KEYBINDS_EXPANDED_HEIGHT + 1
    } else {
        1
    };
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(kb_height),
        ])
        .split(left);
    let servers_rect = v[0];
    let sep1 = v[1];
    let sessions_rect = v[2];
    let sep2 = v[3];
    let keybinds_rect = v[4];

    // Right column: activity (top) over alerts (bottom).
    let rv = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(right);
    let activity_rect = rv[0];
    let sep_r = rv[1];
    let alerts_rect = rv[2];

    let sb = Borders::TOP | Borders::LEFT;
    let mb = Borders::LEFT;
    let atb = Borders::TOP | Borders::RIGHT;
    let alb = Borders::RIGHT | Borders::BOTTOM;
    let servers_inner = inner_of(servers_rect, sb);
    let sessions_inner = inner_of(sessions_rect, mb);
    let activity_inner = inner_of(activity_rect, atb);
    let alerts_inner = inner_of(alerts_rect, alb);

    let server_entries = build_server_entries(app, servers_inner.width, theme, icons);
    let session_entries = build_session_entries(app, sessions_inner.width, theme, icons);
    let activity_entries = build_activity_entries(app, activity_inner.width, theme, icons);
    let alert_entries = build_alert_entries(app, alerts_inner.width, theme, icons);
    let alerts_title = daemon_status(&app.snapshot);

    render_board_into(
        " Servers ",
        focus == Focus::Servers,
        &server_entries,
        SERVER_LPE,
        &mut app.servers,
        servers_rect,
        servers_inner,
        f.buffer_mut(),
        theme,
        sb,
    );
    render_separator(
        sep1.y,
        sep1.x,
        sep1.width,
        " Sessions ",
        theme,
        f.buffer_mut(),
        true,
        false,
    );
    render_board_into(
        "",
        focus == Focus::Sessions,
        &session_entries,
        SESSION_LPE,
        &mut app.sessions,
        sessions_rect,
        sessions_inner,
        f.buffer_mut(),
        theme,
        mb,
    );

    // Keybinds separator + (collapsible) content.
    let kb_title = if app.keybinds_expanded {
        " Keybinds  ? "
    } else {
        " Keybinds  ? to expand "
    };
    render_separator(
        sep2.y,
        sep2.x,
        sep2.width,
        kb_title,
        theme,
        f.buffer_mut(),
        true,
        false,
    );
    let kb_block = panel_block("", false, theme, Borders::BOTTOM | Borders::LEFT);
    let kb_inner = kb_block.inner(keybinds_rect);
    kb_block.render(keybinds_rect, f.buffer_mut());
    if app.keybinds_expanded {
        render_keybinds_content(kb_inner, f.buffer_mut(), theme);
    }

    // Right column: activity board (top), then the alerts board (bottom) with
    // the daemon-status line carried on the separator between them.
    render_board_into(
        " Activity ",
        focus == Focus::Activity,
        &activity_entries,
        ACTIVITY_LPE,
        &mut app.activity,
        activity_rect,
        activity_inner,
        f.buffer_mut(),
        theme,
        atb,
    );
    render_separator(
        sep_r.y,
        sep_r.x,
        sep_r.width,
        &alerts_title,
        theme,
        f.buffer_mut(),
        false,
        true,
    );
    render_board_into(
        "",
        focus == Focus::Alerts,
        &alert_entries,
        ALERT_LPE,
        &mut app.alerts,
        alerts_rect,
        alerts_inner,
        f.buffer_mut(),
        theme,
        alb,
    );

    render_divider_col(
        divider.x,
        divider.y,
        divider.y + divider.height.saturating_sub(1),
        &[sep1.y, sep2.y],
        &[sep_r.y],
        theme.border_unfocused,
        f.buffer_mut(),
    );

    layout.servers = servers_rect;
    layout.servers_inner = servers_inner;
    layout.sessions = sessions_rect;
    layout.sessions_inner = sessions_inner;
    layout.activity = activity_rect;
    layout.activity_inner = activity_inner;
    layout.alerts = alerts_rect;
    layout.alerts_inner = alerts_inner;
    layout.keybinds = Rect {
        y: sep2.y,
        height: sep2.height + keybinds_rect.height,
        ..keybinds_rect
    };
    layout.divider_col = divider.x;
    layout.total_width = area.width;
}

/// Main event loop: re-load the snapshot, render the boards, handle input.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
    watch_rx: Option<&mpsc::Receiver<()>>,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut layout = PanelLayout::default();

    loop {
        terminal.draw(|f| render_dashboard(f, app, &mut layout))?;

        if app.quit {
            return Ok(());
        }

        let timeout = TICK_INTERVAL
            .checked_sub(last_tick.elapsed())
            .unwrap_or_default();

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => handle_key(app, key.code),
                Event::Mouse(mouse) => {
                    handle_mouse(app, mouse.kind, mouse.column, mouse.row, &layout);
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
        Alert, ClientInfo, DaemonSnapshot, LastAction, Milestone, MilestoneKind, Progress,
        ServerEntry, SessionEntry, SessionStatus, Snapshot,
    };
    use crate::tui::data::MockDataSource;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn fixture() -> Snapshot {
        Snapshot {
            schema: 1,
            daemon: DaemonSnapshot {
                instance_id: "daemon:test".to_string(),
                pid: 4242,
                version: "2.0.0".to_string(),
                started_at: "2026-06-08T12:00:00Z".to_string(),
                generated_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            },
            servers: vec![
                ServerEntry {
                    id: "rust-analyzer@/p/Catenary".to_string(),
                    server: "rust-analyzer".to_string(),
                    scope_root: "/p/Catenary".to_string(),
                    state: "probing".to_string(),
                    // 5m05s ago → growing time-in-state.
                    state_since: (chrono::Utc::now() - chrono::Duration::seconds(305))
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    progress: Some(Progress {
                        title: "Indexing".to_string(),
                        message: None,
                        pct: Some(62),
                    }),
                    ..ServerEntry::default()
                },
                ServerEntry {
                    id: "pyright@/p/Other".to_string(),
                    server: "pyright".to_string(),
                    state: "healthy".to_string(),
                    state_since: chrono::Utc::now().to_rfc3339(),
                    ..ServerEntry::default()
                },
            ],
            sessions: vec![SessionEntry {
                id: "mcp:7f3a".to_string(),
                client: ClientInfo {
                    name: "claude".to_string(),
                    version: None,
                },
                status: SessionStatus::Editing,
                last_seen: chrono::Utc::now().to_rfc3339(),
                last_action: Some(LastAction {
                    summary: "edited src/db.rs".to_string(),
                    at: chrono::Utc::now().to_rfc3339(),
                }),
                roots: vec!["/p/Catenary".to_string()],
                ..SessionEntry::default()
            }],
            alerts: vec![Alert {
                at: "2026-06-08T14:32:00.000Z".to_string(),
                level: "error".to_string(),
                source: Some("lsp".to_string()),
                text: "rust-analyzer exited (code 101)".to_string(),
                scope: Some("rust-analyzer@/p/Catenary".to_string()),
            }],
            activity: vec![Milestone {
                at: "2026-06-08T14:31:00.000Z".to_string(),
                kind: MilestoneKind::Diagnostics,
                summary: "3 errors, 12 warnings · 4 files".to_string(),
                scope: Some("mcp:7f3a".to_string()),
            }],
        }
    }

    fn app_for<'a>(theme: &'a Theme, icons: &'a IconSet, snap: Snapshot) -> App<'a> {
        App::new(theme, icons, Box::new(MockDataSource::new(snap))).expect("app")
    }

    fn render_to_string(snap: Snapshot, width: u16, height: u16) -> String {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = app_for(&theme, &icons, snap);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut layout = PanelLayout::default();
        terminal
            .draw(|f| render_dashboard(f, &mut app, &mut layout))
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

    #[test]
    fn renders_all_four_boards_from_fixture() {
        let out = render_to_string(fixture(), 100, 24);
        assert!(out.contains("Servers"), "server board title: {out}");
        assert!(out.contains("rust-analyzer"), "server row: {out}");
        assert!(out.contains("pyright"), "second server: {out}");
        assert!(out.contains("Sessions"), "session board title");
        assert!(out.contains("claude"), "session client");
        assert!(out.contains("edited src/db.rs"), "session last action");
        assert!(out.contains("Activity"), "activity board title: {out}");
        assert!(
            out.contains("3 errors, 12 warnings"),
            "milestone summary: {out}"
        );
        assert!(out.contains("Alerts"), "alerts title");
        assert!(out.contains("rust-analyzer exited"), "alert text");
    }

    #[test]
    fn stuck_probing_shows_growing_time_in_state() {
        let out = render_to_string(fixture(), 100, 24);
        // The probing server's state_since is 5m05s in the past.
        assert!(out.contains("5m05s"), "time-in-state visible: {out}");
        assert!(out.contains("probing"), "probing state shown");
    }

    #[test]
    fn narrow_layout_still_renders_all_boards() {
        let out = render_to_string(fixture(), 50, 32);
        assert!(out.contains("Servers"), "{out}");
        assert!(out.contains("Sessions"), "{out}");
        assert!(out.contains("Activity"), "{out}");
        assert!(out.contains("Alerts"), "{out}");
        assert!(out.contains("rust-analyzer"), "{out}");
    }

    #[test]
    fn waiting_state_when_no_daemon() {
        let out = render_to_string(Snapshot::default(), 80, 12);
        assert!(out.contains("Waiting for daemon"), "{out}");
    }

    #[test]
    fn key_navigation_moves_focus_and_cursor() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let mut app = app_for(&theme, &icons, fixture());
        app.servers.visible = 10;
        assert_eq!(app.focus, Focus::Servers);
        handle_key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.servers.cursor, 1);
        handle_key(&mut app, KeyCode::Tab);
        assert_eq!(app.focus, Focus::Sessions);
        // 'y' on a session yanks without panicking.
        handle_key(&mut app, KeyCode::Char('y'));
    }

    #[test]
    fn watcher_fires_on_rename_into_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let (_watcher, rx) = start_state_watcher(&path).expect("watcher");

        // Atomic write: temp + rename, as the daemon does.
        let tmp = dir.path().join("state.json.tmp");
        std::fs::write(&tmp, "{}").expect("write tmp");
        std::fs::rename(&tmp, &path).expect("rename");

        let got = rx.recv_timeout(Duration::from_secs(5));
        assert!(got.is_ok(), "watcher should signal on rename to state.json");
    }
}
