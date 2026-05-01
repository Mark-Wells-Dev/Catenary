// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Messages panel: state management, navigation, expansion, and rendering.
//!
//! Panel state owns messages, cursor, scroll offset, tail attach/detach,
//! horizontal scrolling, and expansion toggles. Rendering converts
//! [`FlatLine`]s into styled terminal output.
//!
//! Pipeline types and passes live in [`super::pipeline`]. Flat line
//! generation lives in [`super::flat`].

use std::collections::{HashMap, HashSet};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use unicode_width::UnicodeWidthStr;

use super::degradation::{TitleSegment, degrade_title};
use super::flat::FlatLine;
use super::format::{
    format_collapsed_styled, format_message_styled, format_pair_styled, format_scope_styled,
};
use super::icons::IconSet;
use super::selection::VisualSelection;
use super::theme::Theme;
use crate::session::SessionMessage;

/// State for a single messages panel.
pub struct PanelState<'a> {
    /// Session ID this panel is tailing.
    pub session_id: String,
    /// All messages loaded for this session.
    pub messages: Vec<SessionMessage>,
    /// Cursor position (index into flat lines).
    pub cursor: usize,
    /// Scroll offset from top of content.
    pub scroll_offset: usize,
    /// Whether the panel is attached to the tail (auto-scrolling).
    pub tail_attached: bool,
    /// Horizontal scroll offset (for wide lines).
    pub horizontal_scroll: usize,
    /// Whether this panel is pinned (enlarged).
    pub pinned: bool,
    /// Language server names for the title bar.
    pub language_servers: Vec<String>,
    /// Indices of expanded messages (in the messages Vec).
    pub expanded: HashSet<usize>,
    /// Active visual selection, if any.
    pub visual_selection: Option<VisualSelection>,
    /// Last known viewport height (updated each render frame).
    pub viewport_height: usize,
    /// Whether the session is still alive (process running).
    pub alive: bool,
    /// Active filter pattern (case-insensitive substring match).
    pub filter_pattern: Option<String>,
    /// Semantic color theme (borrowed from the application).
    pub theme: &'a Theme,
    /// Resolved icon set (borrowed from the application).
    pub icons: &'a IconSet,
}

// ── Construction & navigation ───────────────────────────────────────────

impl<'a> PanelState<'a> {
    /// Create a new panel for the given session.
    ///
    /// Starts with empty messages, cursor at 0, tail attached, no horizontal
    /// scroll, not pinned.
    #[must_use]
    pub fn new(session_id: String, theme: &'a Theme, icons: &'a IconSet) -> Self {
        Self {
            session_id,
            messages: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            tail_attached: true,
            horizontal_scroll: 0,
            pinned: false,
            alive: true,
            language_servers: Vec::new(),
            expanded: HashSet::new(),
            visual_selection: None,
            viewport_height: 0,
            filter_pattern: None,
            theme,
            icons,
        }
    }

    /// Total number of visible lines (flat lines including expanded detail).
    fn total_lines(&self) -> usize {
        self.flat_lines().len()
    }

    /// Load historical messages. Sets cursor to the last line and attaches tail.
    pub fn load_messages(&mut self, messages: Vec<SessionMessage>) {
        self.messages = messages;
        self.expanded.clear();
        let total = self.total_lines();
        self.cursor = total.saturating_sub(1);
        self.tail_attached = true;
        self.snap_viewport(0);
    }

    /// Append a new message.
    ///
    /// If tail attached, advance cursor and scroll to keep the latest message
    /// visible. If detached, just append (cursor stays put).
    pub fn push_message(&mut self, msg: SessionMessage) {
        self.messages.push(msg);
        if self.tail_attached {
            let total = self.total_lines();
            self.cursor = total.saturating_sub(1);
            self.snap_viewport(0);
        }
    }

    /// Move cursor by `delta`. Clamp to `[0, total_lines - 1]`.
    ///
    /// - Moving up (`delta < 0`): detach tail.
    /// - Moving down past the last line: reattach tail, cursor on last.
    /// - Snap viewport to center cursor (`scrolloff=999` behavior).
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "terminal item counts never overflow isize"
    )]
    pub fn navigate(&mut self, delta: isize) {
        let total = self.total_lines();
        if total == 0 {
            self.cursor = 0;
            return;
        }

        let max = (total - 1) as isize;
        let new_pos = self.cursor as isize + delta;

        if delta < 0 {
            self.tail_attached = false;
        }

        if new_pos > max {
            // Moved past end — reattach.
            self.cursor = total - 1;
            self.tail_attached = true;
        } else {
            self.cursor = new_pos.max(0) as usize;
        }

        self.snap_viewport(0);
    }

    /// Scroll viewport by `delta` lines without moving the cursor.
    ///
    /// Used for mouse wheel: moves `scroll_offset` only, detaches tail on
    /// scroll-up, reattaches when scrolled to the very bottom.
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "terminal item counts never overflow isize"
    )]
    pub fn scroll_viewport(&mut self, delta: isize) {
        let total = self.total_lines();
        if total == 0 {
            return;
        }

        if delta < 0 {
            self.tail_attached = false;
        }

        let new_offset = (self.scroll_offset as isize + delta)
            .max(0)
            .min(total.saturating_sub(1) as isize);

        #[allow(clippy::cast_sign_loss, reason = "clamped to >= 0")]
        {
            self.scroll_offset = new_offset as usize;
        }

        // Reattach tail if scrolled to the very bottom.
        let vh = if self.viewport_height > 0 {
            self.viewport_height
        } else {
            20
        };
        if self.scroll_offset + vh >= total {
            self.tail_attached = true;
        }
    }

    /// Jump to first line — `g` key.
    pub const fn scroll_to_top(&mut self) {
        self.cursor = 0;
        self.scroll_offset = 0;
        self.tail_attached = false;
    }

    /// Jump to last line — `G` key.
    pub fn scroll_to_bottom(&mut self) {
        let total = self.total_lines();
        self.cursor = total.saturating_sub(1);
        self.tail_attached = true;
        self.snap_viewport(0);
    }

    /// Page up — `Ctrl+U`.
    pub fn page_up(&mut self, height: usize) {
        let half = (height / 2).max(1);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "terminal heights never overflow isize"
        )]
        self.navigate(-(half as isize));
    }

    /// Scroll horizontally by `delta` columns.
    ///
    /// Positive delta scrolls right, negative scrolls left. Clamps to zero.
    #[allow(
        clippy::cast_sign_loss,
        reason = "delta is checked positive before cast"
    )]
    pub const fn scroll_horizontal(&mut self, delta: isize) {
        if delta < 0 {
            self.horizontal_scroll = self.horizontal_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.horizontal_scroll = self.horizontal_scroll.saturating_add(delta as usize);
        }
    }

    /// Page down — `Ctrl+D`.
    pub fn page_down(&mut self, height: usize) {
        let half = (height / 2).max(1);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "terminal heights never overflow isize"
        )]
        self.navigate(half as isize);
    }

    /// Compute the `(start, end)` indices of lines visible in the viewport.
    ///
    /// `height` is the inner content height (excluding title bar and borders).
    #[must_use]
    pub fn visible_range(&self, height: usize) -> (usize, usize) {
        let total = self.total_lines();
        let start = self.scroll_offset.min(total);
        let end = (start + height).min(total);
        (start, end)
    }

    /// Derive language server names from messages.
    ///
    /// Scans for LSP messages and tracks unique server names.
    pub fn update_language_servers(&mut self) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::new();

        for msg in &self.messages {
            if msg.r#type == "lsp" && !msg.server.is_empty() && seen.insert(msg.server.clone()) {
                order.push(msg.server.clone());
            }
        }

        self.language_servers = order;
    }

    /// Snap viewport so cursor is centered (`scrolloff=999` behavior).
    ///
    /// `height` of 0 means use the last known viewport height from the
    /// previous render frame. Falls back to 20 if never rendered.
    fn snap_viewport(&mut self, height: usize) {
        let h = if height > 0 {
            height
        } else if self.viewport_height > 0 {
            self.viewport_height
        } else {
            20
        };
        let total = self.total_lines();
        if total <= h {
            self.scroll_offset = 0;
            return;
        }
        let target = self.cursor.saturating_sub(h / 2);
        self.scroll_offset = target.min(total.saturating_sub(h));
    }

    /// Toggle expansion of the message under the cursor.
    ///
    /// - On a `MessageHeader`: toggle the message in/out of `expanded`.
    /// - On a `Detail` line: collapse the parent message, move cursor to its header.
    pub fn toggle_expansion(&mut self) {
        let flat = self.flat_lines();
        let Some(current) = flat.get(self.cursor) else {
            return;
        };
        match *current {
            FlatLine::MessageHeader { message_index, .. } => {
                if self.expanded.contains(&message_index) {
                    self.expanded.remove(&message_index);
                } else {
                    self.expanded.insert(message_index);
                }
            }
            FlatLine::Detail { message_index, .. } => {
                self.expanded.remove(&message_index);
                // Move cursor to the parent header.
                let new_flat = self.flat_lines();
                if let Some(pos) = new_flat.iter().position(|fl| {
                    matches!(fl, FlatLine::MessageHeader { message_index: mi, .. } if *mi == message_index)
                }) {
                    self.cursor = pos;
                }
            }
            FlatLine::CollapsedHeader { start_index, .. } => {
                if self.expanded.contains(&start_index) {
                    self.expanded.remove(&start_index);
                } else {
                    self.expanded.insert(start_index);
                }
            }
            FlatLine::ScopeHeader { expansion_key, .. } => {
                if self.expanded.contains(&expansion_key) {
                    self.expanded.remove(&expansion_key);
                } else {
                    self.expanded.insert(expansion_key);
                }
            }
            FlatLine::Separator => {
                // Separators only appear inside ScopeChild; bare separator is inert.
            }
            FlatLine::ScopeChild {
                scope_parent_index,
                ref inner,
                ..
            } => match inner.as_ref() {
                FlatLine::CollapsedHeader { start_index, .. } => {
                    if self.expanded.contains(start_index) {
                        self.expanded.remove(start_index);
                    } else {
                        self.expanded.insert(*start_index);
                    }
                }
                FlatLine::ScopeHeader { expansion_key, .. } => {
                    if self.expanded.contains(expansion_key) {
                        self.expanded.remove(expansion_key);
                    } else {
                        self.expanded.insert(*expansion_key);
                    }
                }
                _ => {
                    // MessageHeader, Detail, Separator — collapse parent scope.
                    self.expanded.remove(&scope_parent_index);
                    let new_flat = self.flat_lines();
                    if let Some(pos) = new_flat.iter().position(|fl| {
                        matches!(fl, FlatLine::ScopeHeader { expansion_key, .. } if *expansion_key == scope_parent_index)
                    }) {
                        self.cursor = pos;
                    }
                }
            },
        }
        self.snap_viewport(0);
    }
}

// ── Expansion helpers ───────────────────────────────────────────────────

/// Generate styled frontmatter lines for an expanded message.
///
/// Shows only the pretty-printed payload JSON with 10-space indent and
/// muted style. No method header or separator — the method is already
/// visible in the summary line.
///
/// Returns an empty vec for messages with empty payloads.
#[must_use]
pub fn frontmatter_lines(msg: &SessionMessage, theme: &Theme) -> Vec<Line<'static>> {
    let payload = &msg.payload;
    if payload.as_object().is_none_or(serde_json::Map::is_empty) {
        return Vec::new();
    }

    // Indent to align past the timestamp column ("HH:MM:SS  " = 10 chars).
    let indent = "          ";
    let mut lines = Vec::new();

    if let Ok(pretty) = serde_json::to_string_pretty(payload) {
        for line in pretty.lines() {
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(line.to_string(), theme.muted),
            ]));
        }
    }

    lines
}

// ── Rendering ───────────────────────────────────────────────────────────

/// Build the title line for a panel, fitting to the available width.
///
/// Uses [`degrade_title`] for progressive truncation of the session ID
/// portion, then appends language server info if there's remaining width.
fn build_title(state: &PanelState<'_>, max_width: u16) -> Line<'static> {
    let id_style = if state.alive {
        state.theme.session_active
    } else {
        state.theme.session_dead
    };

    // Build the core title (session ID + chrome) via degradation.
    // Reserve 1 column for the trailing space after the title.
    let title_budget = max_width.saturating_sub(1);
    let segments = degrade_title(&state.session_id, None, title_budget);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used_width: usize = 0;

    for seg in &segments {
        match seg {
            TitleSegment::Chrome(text) => {
                used_width += UnicodeWidthStr::width(text.as_str());
                spans.push(Span::raw(text.clone()));
            }
            TitleSegment::SessionId(text) => {
                used_width += UnicodeWidthStr::width(text.as_str());
                spans.push(Span::styled(text.clone(), id_style));
            }
        }
    }

    // Append language server info if there's remaining width.
    let remaining = (max_width as usize).saturating_sub(used_width);
    append_ls_info(&mut spans, state, remaining);

    // Trailing space.
    spans.push(Span::raw(" ".to_string()));
    Line::from(spans)
}

/// Append language server info to the title spans, fitting to `budget` columns.
///
/// Progressive degradation:
/// 1. Icons + names with `╱` separators (e.g., `● rust ╱ ● ts`)
/// 2. Names with `╱` separators (e.g., `rust ╱ ts`)
/// 3. Names space-separated (e.g., `rust ts`)
/// 4. Nothing (not enough room)
fn append_ls_info(spans: &mut Vec<Span<'static>>, state: &PanelState<'_>, budget: usize) {
    if state.language_servers.is_empty() || budget < 5 {
        return;
    }

    let style = Style::default().fg(Color::Green);
    let icon = &state.icons.ls_active;
    let sep = " \u{2571} "; // ╱

    // Compute widths for each degradation level.
    let with_icons_width: usize = state
        .language_servers
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let sep_w = if i > 0 {
                UnicodeWidthStr::width(sep)
            } else {
                0
            };
            sep_w + UnicodeWidthStr::width(icon.as_str()) + UnicodeWidthStr::width(name.as_str())
        })
        .sum::<usize>()
        + 1; // leading space

    let names_sep_width: usize = state
        .language_servers
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let sep_w = if i > 0 {
                UnicodeWidthStr::width(sep)
            } else {
                0
            };
            sep_w + UnicodeWidthStr::width(name.as_str())
        })
        .sum::<usize>()
        + 1;

    let names_space_width: usize = state
        .language_servers
        .iter()
        .enumerate()
        .map(|(i, name)| usize::from(i > 0) + UnicodeWidthStr::width(name.as_str()))
        .sum::<usize>()
        + 1;

    if with_icons_width <= budget {
        // Level 1: icons + fancy separators.
        spans.push(Span::raw(" ".to_string()));
        for (i, name) in state.language_servers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(sep.to_string()));
            }
            spans.push(Span::styled(icon.clone(), style));
            spans.push(Span::styled(name.clone(), style));
        }
    } else if names_sep_width <= budget {
        // Level 2: names + fancy separators.
        spans.push(Span::raw(" ".to_string()));
        for (i, name) in state.language_servers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(sep.to_string()));
            }
            spans.push(Span::styled(name.clone(), style));
        }
    } else if names_space_width <= budget {
        // Level 3: names space-separated.
        spans.push(Span::raw(" ".to_string()));
        for (i, name) in state.language_servers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" ".to_string()));
            }
            spans.push(Span::styled(name.clone(), style));
        }
    }
    // Level 4: nothing fits — don't append anything.
}

/// Convert a borrowed `Line` into a fully owned `Line<'static>`.
#[must_use]
pub fn to_owned_line(line: &Line<'_>) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.to_string(), s.style))
            .collect::<Vec<_>>(),
    )
}

/// Apply horizontal scrolling to a styled line, inserting clip indicators.
///
/// Returns a new line clipped to `width` display columns, with `◀…` on the
/// left when content is clipped left and `…▶` on the right when clipped right.
fn clip_line_horizontal(line: &Line<'_>, h_scroll: usize, width: usize) -> Line<'static> {
    if width < 4 {
        // Too narrow for indicators, just return empty.
        return Line::default();
    }

    // Flatten the line into a single plain string + collect grapheme-aware info.
    let full_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let full_width = UnicodeWidthStr::width(full_text.as_str());

    if full_width == 0 {
        return Line::default();
    }

    // No clipping needed at all.
    if h_scroll == 0 && full_width <= width {
        return to_owned_line(line);
    }

    let clipped_left = h_scroll > 0;
    let clipped_right = full_width > h_scroll + width;

    // Calculate available content width after reserving indicator space.
    let left_reserve = if clipped_left { 2 } else { 0 };
    let right_reserve = if clipped_right { 2 } else { 0 };
    let content_width = width.saturating_sub(left_reserve + right_reserve);

    if content_width == 0 {
        let mut spans = Vec::new();
        if clipped_left {
            spans.push(Span::styled(
                "\u{25C0}\u{2026}",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if clipped_right {
            spans.push(Span::styled(
                "\u{2026}\u{25B6}",
                Style::default().fg(Color::DarkGray),
            ));
        }
        return Line::from(spans);
    }

    // Walk through spans, tracking display-width position, and extract the
    // visible portion respecting h_scroll and content_width.
    let vis_start = h_scroll;
    let vis_end = h_scroll + content_width;

    let mut result_spans: Vec<Span<'static>> = Vec::new();

    if clipped_left {
        result_spans.push(Span::styled(
            "\u{25C0}\u{2026}",
            Style::default().fg(Color::DarkGray),
        )); // ◀…
    }

    // Walk spans and extract visible portion.
    let mut col = 0usize;
    for span in &line.spans {
        let span_text = span.content.as_ref();
        let span_width = UnicodeWidthStr::width(span_text);
        let span_end = col + span_width;

        if span_end <= vis_start || col >= vis_end {
            // Entirely outside visible range.
            col = span_end;
            continue;
        }

        // Partially or fully visible. Extract the visible chars.
        let mut visible = String::new();
        let mut char_col = col;
        for ch in span_text.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let ch_end = char_col + ch_width;
            if char_col >= vis_end {
                break;
            }
            if ch_end > vis_start {
                visible.push(ch);
            }
            char_col = ch_end;
        }

        if !visible.is_empty() {
            result_spans.push(Span::styled(visible, span.style));
        }

        col = span_end;
    }

    if clipped_right {
        result_spans.push(Span::styled(
            "\u{2026}\u{25B6}",
            Style::default().fg(Color::DarkGray),
        )); // …▶
    }

    Line::from(result_spans)
}

/// Render a single `FlatLine` into a styled `Line`.
///
/// Shared between the top-level render loop and `ScopeChild` indentation.
fn render_flat_line_styled(
    fl: &FlatLine,
    state: &PanelState<'_>,
    detail_cache: &mut HashMap<usize, Vec<Line<'static>>>,
) -> Line<'static> {
    match fl {
        FlatLine::MessageHeader {
            message_index,
            paired_response,
        } => paired_response.map_or_else(
            || format_message_styled(&state.messages[*message_index], state.icons, state.theme),
            |resp_idx| {
                format_pair_styled(
                    &state.messages[*message_index],
                    &state.messages[resp_idx],
                    state.icons,
                    state.theme,
                )
            },
        ),
        FlatLine::Detail {
            message_index,
            detail_index,
        } => detail_cache
            .entry(*message_index)
            .or_insert_with(|| frontmatter_lines(&state.messages[*message_index], state.theme))
            .get(*detail_index)
            .cloned()
            .unwrap_or_default(),
        FlatLine::CollapsedHeader {
            start_index,
            end_index,
            count,
        } => format_collapsed_styled(
            &state.messages,
            *start_index,
            *end_index,
            *count,
            state.icons,
            state.theme,
        ),
        FlatLine::ScopeHeader {
            parent,
            child_count,
            position,
            ..
        } => format_scope_styled(
            parent,
            *child_count,
            *position,
            &state.messages,
            state.icons,
            state.theme,
        ),
        FlatLine::Separator => {
            let indent = "          "; // 10 spaces
            Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled("---".to_string(), state.theme.muted),
            ])
        }
        FlatLine::ScopeChild { depth, inner, .. } => {
            let indent = " ".repeat(depth * 4);
            let inner_line = render_flat_line_styled(inner, state, detail_cache);
            let mut spans = vec![Span::raw(indent)];
            spans.extend(
                inner_line
                    .spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style)),
            );
            Line::from(spans)
        }
    }
}

/// Render a single messages panel into the given buffer area.
///
/// The panel owns its top row (title bar) and right column (scrollbar,
/// rendered by ticket 06). Left and bottom edges are content. The caller
/// (grid) handles junction characters.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "terminal coordinates are always small; pair merge adds detail lookup logic"
)]
pub fn render_panel(
    state: &PanelState<'_>,
    area: Rect,
    buf: &mut Buffer,
    focused: bool,
    show_borders: bool,
) {
    if area.width < 2 || area.height < 1 {
        return;
    }

    let border_style = if focused {
        state.theme.border_focused
    } else {
        state.theme.border_unfocused
    };

    let border_set = if focused {
        symbols::border::THICK
    } else {
        symbols::border::PLAIN
    };

    // When borders are shown, the top row is the title bar. When hidden,
    // only the right-column border is drawn (for grid panel separation).
    let borders = if show_borders {
        Borders::TOP | Borders::RIGHT
    } else {
        Borders::RIGHT
    };

    let mut block = Block::default()
        .borders(borders)
        .border_set(border_set)
        .border_style(border_style);

    if show_borders {
        let title_style = if focused {
            state.theme.title
        } else {
            state.theme.border_unfocused
        };
        let title_width = area.width.saturating_sub(1); // right border
        let title = build_title(state, title_width);
        block = block.title(title).title_style(title_style);
    }

    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width < 2 || inner.height < 1 {
        return;
    }

    // Build flat line list (headers + expanded detail lines).
    let flat = state.flat_lines();

    // Viewport slicing.
    let height = inner.height as usize;
    let total = flat.len();
    let start = state.scroll_offset.min(total);
    let end = (start + height).min(total);

    // Cache detail lines per expanded message to avoid recomputation.
    let mut detail_cache: HashMap<usize, Vec<Line<'static>>> = HashMap::new();

    // Render each visible line.
    let content_width = inner.width as usize;
    for (i, fl) in flat[start..end].iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let line = render_flat_line_styled(fl, state, &mut detail_cache);

        let display_line = if state.horizontal_scroll > 0
            || UnicodeWidthStr::width(
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .as_str(),
            ) > content_width
        {
            clip_line_horizontal(&line, state.horizontal_scroll, content_width)
        } else {
            to_owned_line(&line)
        };

        // Apply cursor highlight to the entire row.
        let line_index = start + i;
        if line_index == state.cursor {
            // Set selection style on the entire row first.
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(state.theme.selection);
            }
        }

        buf.set_line(inner.x, y, &display_line, inner.width);

        // Re-apply selection style on top of content for cursor row.
        if line_index == state.cursor {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(state.theme.selection);
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::IconConfig;
    use crate::session::SessionMessage;
    use crate::tui::format::format_scope_plain;
    use crate::tui::pipeline::{DisplayEntry, SegmentPosition};

    fn test_theme() -> Theme {
        Theme::new()
    }

    fn test_icons() -> IconSet {
        IconSet::from_config(IconConfig::default())
    }

    use crate::session::test_support;

    fn make_message(r#type: &str, method: &str, server: &str) -> SessionMessage {
        test_support::message(r#type, method, server)
    }

    /// Create N messages that never collapse (hook messages have `None` collapse key).
    fn make_non_collapsing_messages(n: usize) -> Vec<SessionMessage> {
        (0..n)
            .map(|i| make_message("hook", &format!("test-{i}"), "catenary"))
            .collect()
    }

    fn make_message_with_payload(
        r#type: &str,
        method: &str,
        server: &str,
        payload: serde_json::Value,
    ) -> SessionMessage {
        test_support::message_with_payload(r#type, method, server, payload)
    }

    /// An LSP message with a non-empty payload (expandable).
    fn make_lsp_message() -> SessionMessage {
        make_message_with_payload(
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            serde_json::json!({"id": 1, "method": "textDocument/hover", "params": {}}),
        )
    }

    /// A hook diagnostic message with preview (expandable).
    fn make_hook_diag_message(file: &str, count: u64) -> SessionMessage {
        make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({
                "file": file,
                "count": count,
                "preview": "\t:12:1 [error] rustc: something"
            }),
        )
    }

    /// Convert a ratatui buffer to a single string for assertion matching.
    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                s.push_str(cell.symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn test_panel_new_tail_attached() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("abc123".to_string(), &theme, &icons);
        assert!(panel.tail_attached);
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert_eq!(panel.horizontal_scroll, 0);
        assert!(!panel.pinned);
        assert!(panel.messages.is_empty());
    }

    #[test]
    fn test_panel_load_messages() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(10);
        panel.load_messages(messages);
        assert_eq!(panel.messages.len(), 10);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_message_attached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);
        assert_eq!(panel.cursor, 4);

        panel.push_message(make_message("mcp", "tools/list", "catenary"));
        assert_eq!(panel.messages.len(), 6);
        assert_eq!(panel.cursor, 5);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_message_detached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);

        // Navigate up to detach.
        panel.navigate(-1);
        assert!(!panel.tail_attached);
        let cursor_before = panel.cursor;

        panel.push_message(make_message("mcp", "tools/list", "catenary"));
        assert_eq!(panel.messages.len(), 6);
        assert_eq!(panel.cursor, cursor_before);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_navigate_up_detaches() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(10);
        panel.load_messages(messages);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);

        panel.navigate(-1);
        assert_eq!(panel.cursor, 8);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_navigate_down_past_end_reattaches() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);

        // Navigate up to detach.
        panel.navigate(-2);
        assert_eq!(panel.cursor, 2);
        assert!(!panel.tail_attached);

        // Navigate down past the end.
        panel.navigate(1);
        assert_eq!(panel.cursor, 3);
        panel.navigate(1);
        assert_eq!(panel.cursor, 4);
        // One more should clamp and reattach.
        panel.navigate(1);
        assert_eq!(panel.cursor, 4);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_scroll_to_top() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(20);
        panel.load_messages(messages);

        panel.scroll_to_top();
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_scroll_to_bottom() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(20);
        panel.load_messages(messages);

        panel.scroll_to_top();
        assert!(!panel.tail_attached);

        panel.scroll_to_bottom();
        assert_eq!(panel.cursor, 19);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_visible_range() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(100);
        panel.load_messages(messages);

        // Move cursor to 50 and snap viewport.
        panel.cursor = 50;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        // Cursor at 50, centered in height 20 → offset ~40.
        assert_eq!(start, 40);
        assert_eq!(end, 60);
    }

    #[test]
    fn test_panel_visible_range_at_top() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(100);
        panel.load_messages(messages);

        // Cursor near top — can't center.
        panel.cursor = 3;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        assert_eq!(start, 0);
        assert_eq!(end, 20);
    }

    #[test]
    fn test_panel_visible_range_at_bottom() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(100);
        panel.load_messages(messages);

        // Cursor near bottom.
        panel.cursor = 97;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        assert_eq!(end, 100);
        assert_eq!(start, 80);
    }

    #[test]
    fn test_panel_render_messages() {
        let theme = test_theme();
        let icons = test_icons();
        // Use a hook message in between to break any potential collapse.
        let messages: Vec<SessionMessage> = vec![
            make_message_with_payload(
                "mcp",
                "tools/call",
                "catenary",
                serde_json::json!({"params": {"name": "grep"}}),
            ),
            make_message("hook", "break", "catenary"),
            make_message_with_payload(
                "mcp",
                "tools/call",
                "catenary",
                serde_json::json!({"params": {"name": "glob"}}),
            ),
        ];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("grep"), "expected grep tool name");
        assert!(content.contains("glob"), "expected glob tool name");
    }

    #[test]
    fn test_panel_render_empty() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("empty123".to_string(), &theme, &icons);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should contain the title, no panic.
        assert!(content.contains("Events"), "expected title in empty panel");
    }

    #[test]
    fn test_panel_render_cursor_highlight() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = make_non_collapsing_messages(5);

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Set cursor to row 1 (second message in visible area).
        panel.cursor = 1;
        panel.snap_viewport(8);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // The cursor row (row 1 in content = y=2 in buffer since y=0 is border,
        // y=1 is first content row, y=2 is second content row).
        // With cursor at index 1 and scroll_offset 0, cursor is at visible row 1.
        // Inner area starts at y=1 (after top border), so cursor row is at y=2.
        let cursor_y = 2u16;
        let inner_x = 1u16; // after left border
        let cell = &buf[(inner_x, cursor_y)];
        // The selection style uses REVERSED modifier.
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::REVERSED),
            "expected REVERSED modifier on cursor row"
        );
    }

    #[test]
    fn test_panel_language_server_status() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        panel.messages = vec![
            make_message("lsp", "textDocument/hover", "rust-analyzer"),
            make_message(
                "lsp",
                "textDocument/completion",
                "typescript-language-server",
            ),
        ];

        panel.update_language_servers();
        assert_eq!(panel.language_servers.len(), 2);
        assert_eq!(panel.language_servers[0], "rust-analyzer");
        assert_eq!(panel.language_servers[1], "typescript-language-server");
    }

    // ── Expansion tests ─────────────────────────────────────────────────

    #[test]
    fn test_toggle_expansion_header() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
        ];
        panel.load_messages(messages);
        // Cursor on message 1 (the expandable LSP message).
        panel.cursor = 1;

        panel.toggle_expansion();
        assert!(panel.expanded.contains(&1));

        panel.toggle_expansion();
        assert!(!panel.expanded.contains(&1));
    }

    #[test]
    fn test_toggle_expansion_detail() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);
        // Find a detail line index.
        let flat = panel.flat_lines();
        let detail_pos = flat
            .iter()
            .position(|fl| {
                matches!(
                    fl,
                    FlatLine::Detail {
                        message_index: 1,
                        ..
                    }
                )
            })
            .expect("should have detail lines");
        panel.cursor = detail_pos;

        panel.toggle_expansion();
        assert!(!panel.expanded.contains(&1));
        // After collapse: cursor should be on message 1's header.
        assert_eq!(panel.cursor, 1);
    }

    #[test]
    fn test_toggle_expansion_empty_payload() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        // Empty payload — expansion is allowed but produces zero detail lines.
        let messages = vec![make_message("lsp", "initialized", "rust-analyzer")];
        panel.load_messages(messages);
        panel.cursor = 0;

        panel.toggle_expansion();
        assert!(panel.expanded.contains(&0));
        // Flat lines: header only (no detail lines for empty payload).
        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 1);
    }

    #[test]
    fn test_cursor_walks_detail_lines() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        panel.cursor = 0;
        panel.tail_attached = false;

        // Walk through all lines one by one.
        for expected in flat.iter().skip(1) {
            panel.navigate(1);
            let current_flat = panel.flat_lines();
            assert_eq!(current_flat[panel.cursor], *expected);
        }
    }

    #[test]
    fn test_frontmatter_lines_non_empty_payload() {
        let msg = make_lsp_message();
        let theme = test_theme();

        let lines = frontmatter_lines(&msg, &theme);
        assert!(!lines.is_empty(), "non-empty payload should have lines");
        // Payload content should be present.
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            all_text.contains("textDocument/hover"),
            "should contain payload content"
        );
        // No method [type] header line.
        assert!(
            !all_text.contains("textDocument/hover [lsp]"),
            "should not contain method [type] header"
        );
        // No box-drawing separator.
        assert!(
            !all_text.contains('\u{2500}'),
            "should not contain box-drawing separator"
        );
        // All lines use muted style.
        for line in &lines {
            for span in &line.spans {
                if !span.content.trim().is_empty() {
                    assert_eq!(
                        span.style, theme.muted,
                        "content spans should use muted style"
                    );
                }
            }
        }
    }

    #[test]
    fn test_frontmatter_lines_empty_payload() {
        let msg = make_message("lsp", "initialized", "rust-analyzer");
        let theme = test_theme();

        let lines = frontmatter_lines(&msg, &theme);
        assert!(
            lines.is_empty(),
            "empty payload should have no frontmatter lines"
        );
    }

    #[test]
    fn test_frontmatter_lines_format() {
        let theme = test_theme();
        let msg = make_message_with_payload(
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            serde_json::json!({"id": 1, "method": "textDocument/hover"}),
        );

        let lines = frontmatter_lines(&msg, &theme);
        // Line count matches pretty-printed JSON.
        let pretty = serde_json::to_string_pretty(&msg.payload).expect("serialize");
        assert_eq!(
            lines.len(),
            pretty.lines().count(),
            "line count should match pretty-printed JSON"
        );

        // Each line: 10-space indent + muted-styled content.
        for line in &lines {
            assert_eq!(
                line.spans.len(),
                2,
                "each line should have indent + content"
            );
            let indent_text: &str = &line.spans[0].content;
            assert_eq!(indent_text, "          ", "indent should be 10 spaces");
            assert_eq!(
                line.spans[1].style, theme.muted,
                "content should use muted style"
            );
        }

        // No method [type] header.
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            !all_text.contains("[lsp]"),
            "should not contain [type] header"
        );
        // No separators.
        assert!(
            !all_text.contains('\u{2500}'),
            "should not contain box-drawing separator"
        );
        assert!(
            !all_text.contains("---"),
            "should not contain --- separator"
        );
    }

    #[test]
    fn test_visible_range_with_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let mut messages = make_non_collapsing_messages(10);
        // Replace message 5 with an expandable one.
        messages[5] = make_lsp_message();
        panel.load_messages(messages);
        panel.expanded.insert(5);

        let flat = panel.flat_lines();
        let detail_count = frontmatter_lines(&panel.messages[5], &theme).len();
        // 10 headers + detail lines for message 5
        assert_eq!(flat.len(), 10 + detail_count);

        // Set cursor to 0, snap viewport.
        panel.cursor = 0;
        panel.snap_viewport(10);
        let (start, end) = panel.visible_range(10);
        assert_eq!(start, 0);
        assert_eq!(end, 10);
    }

    #[test]
    fn test_render_expanded_message() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![make_hook_diag_message("/src/lib.rs", 2)];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);
        panel.cursor = 0;
        panel.snap_viewport(8);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Header should show the diagnostics summary.
        assert!(content.contains("lib.rs"), "expected file name in header");
        // Detail lines should contain the payload (no method header).
        assert!(
            content.contains("count"),
            "expected payload content in detail"
        );
    }

    // ── Format tests ──────────────────────────────────────────────────

    fn make_message_with_id(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
    ) -> SessionMessage {
        test_support::message_with_ids(id, r#type, method, server, request_id, None)
    }

    fn make_message_with_id_ts(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        timestamp: chrono::DateTime<chrono::Utc>,
        payload: serde_json::Value,
    ) -> SessionMessage {
        SessionMessage {
            timestamp,
            ..test_support::message_with_ids_payload(
                id, r#type, method, server, request_id, None, payload,
            )
        }
    }

    #[test]
    fn test_pair_merge_cancellation() {
        use super::super::pipeline::DisplayEntry;
        use super::super::pipeline::pair_merge;

        let messages = vec![
            make_message_with_id(1, "mcp", "tools/call", "catenary", Some(100)),
            make_message_with_id(2, "mcp", "notifications/cancelled", "catenary", Some(100)),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            DisplayEntry::Paired {
                request_index: 0,
                response_index: 1,
                parent_id: None,
            }
        );

        // Verify rendering uses cancelled icon.
        let theme = test_theme();
        let icons = test_icons();
        let line =
            super::super::format::format_pair_styled(&messages[0], &messages[1], &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2501}"),
            "cancellation should show ━ icon: {text}"
        );
        assert!(
            text.contains("cancelled"),
            "cancellation should show cancelled text: {text}"
        );
    }

    #[test]
    fn test_format_pair_styled_timing() {
        use chrono::{TimeDelta, Utc};

        let now = Utc::now();
        let later = now + TimeDelta::milliseconds(1500);
        let theme = test_theme();
        let icons = test_icons();

        let request = make_message_with_id_ts(
            1,
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            None,
            now,
            serde_json::json!({}),
        );
        let response = make_message_with_id_ts(
            2,
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            Some(1),
            later,
            serde_json::json!({"result": null}),
        );

        let line = super::super::format::format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1.5s"), "should contain timing delta: {text}");
    }

    #[test]
    fn test_format_pair_styled_lsp() {
        let theme = test_theme();
        let icons = test_icons();

        let request = make_message_with_id(1, "lsp", "textDocument/hover", "rust-analyzer", None);
        let mut response =
            make_message_with_id(2, "lsp", "textDocument/hover", "rust-analyzer", Some(1));
        response.payload = serde_json::json!({"result": {"contents": "fn main()"}});

        let line = super::super::format::format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[rust-analyzer]"),
            "should contain server name: {text}"
        );
        assert!(
            text.contains("\u{2714}"),
            "LSP success should show ✔ icon: {text}"
        );
        assert!(
            text.contains("textDocument/hover"),
            "should contain method: {text}"
        );
        assert!(!text.contains("<->"), "should not contain arrow: {text}");
    }

    #[test]
    fn test_format_pair_styled_mcp() {
        let theme = test_theme();
        let icons = test_icons();

        let request = make_message_with_id_ts(
            1,
            "mcp",
            "tools/call",
            "catenary",
            None,
            chrono::Utc::now(),
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_id_ts(
            2,
            "mcp",
            "tools/call",
            "catenary",
            Some(1),
            chrono::Utc::now(),
            serde_json::json!({"result": {"content": [{"type": "text", "text": "results"}]}}),
        );

        let line = super::super::format::format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("grep"), "should contain tool name: {text}");
        assert!(
            text.contains("\u{2B9E}"),
            "MCP tool success should show tool icon ⮞: {text}"
        );
        assert!(!text.contains("<->"), "should not contain arrow: {text}");
    }

    // ── Scope tests (PanelState integration) ──────────────────────────

    fn make_message_with_id_parent(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
    ) -> SessionMessage {
        test_support::message_with_ids(id, r#type, method, server, request_id, parent_id)
    }

    #[test]
    fn test_scope_toggle_expansion() {
        // Toggle on ScopeHeader adds/removes from expanded.
        // Toggle on ScopeChild collapses parent, moves cursor.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Cursor on the ScopeHeader (line 0).
        panel.cursor = 0;

        // Toggle: expand scope (expansion key is parent's index = 0).
        panel.toggle_expansion();
        assert!(
            panel.expanded.contains(&0),
            "scope should be expanded after toggle"
        );
        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 3, "expanded should show 3 lines");

        // Toggle again: collapse scope.
        panel.cursor = 0;
        panel.toggle_expansion();
        assert!(
            !panel.expanded.contains(&0),
            "scope should be collapsed after second toggle"
        );

        // Expand again, then toggle on a child.
        panel.cursor = 0;
        panel.toggle_expansion();
        assert!(panel.expanded.contains(&0));
        // Move cursor to first ScopeChild (line 1).
        panel.cursor = 1;
        panel.toggle_expansion();
        assert!(
            !panel.expanded.contains(&0),
            "toggling on ScopeChild should collapse parent"
        );
        assert_eq!(
            panel.cursor, 0,
            "cursor should move to ScopeHeader after child toggle"
        );
    }

    #[test]
    fn test_scope_render_basic() {
        // Render a panel with a scope. Verify the tool name appears in the output.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            {
                let mut m = make_message_with_id_parent(
                    1,
                    "mcp",
                    "tools/call",
                    "catenary",
                    Some(500),
                    None,
                );
                m.payload = serde_json::json!({"params": {"name": "grep"}});
                m
            },
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
        ];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("grep"),
            "expected grep tool name in scope header: {content}"
        );
        assert!(
            content.contains("2 children"),
            "expected child count in scope header: {content}"
        );
    }

    // ── Segmented scope tests ──────────────────────────────────────────

    #[test]
    fn test_segmented_scope_rendering() {
        // Verify ellipsis convention: First → "grep…", Middle → "…grep…",
        // Last → "…grep (metrics)".
        let theme = test_theme();
        let icons = test_icons();
        let make_tool_call = || {
            let mut m =
                make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None);
            m.payload = serde_json::json!({"params": {"name": "grep"}});
            m
        };

        let parent_entry = DisplayEntry::Single {
            index: 0,
            parent_id: None,
        };
        let parent_rc = Rc::new(parent_entry);
        let messages = vec![make_tool_call()];

        // First segment: "grep…"
        let line = format_scope_styled(
            &parent_rc,
            3,
            SegmentPosition::First,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("grep\u{2026}"),
            "First should render grep…: {text}"
        );
        assert!(
            !text.contains("grep\u{2026}\u{2026}"),
            "First should not have double ellipsis: {text}"
        );

        // Middle segment: "…grep…"
        let line = format_scope_styled(
            &parent_rc,
            2,
            SegmentPosition::Middle,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2026}grep\u{2026}"),
            "Middle should render …grep…: {text}"
        );

        // Last segment: "…grep" with metrics
        let line = format_scope_styled(
            &parent_rc,
            1,
            SegmentPosition::Last,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2026}grep"),
            "Last should render …grep: {text}"
        );

        // Only segment: "grep" without ellipsis
        let line = format_scope_styled(
            &parent_rc,
            5,
            SegmentPosition::Only,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("grep"), "Only should contain grep: {text}");
        assert!(
            !text.contains('\u{2026}'),
            "Only should not contain ellipsis: {text}"
        );
    }

    #[test]
    fn test_segmented_scope_independent_expansion() {
        // Expand segment 1, verify segment 2 remains collapsed.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            // Segment 1 children
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
            // Root interruption
            make_message_with_id_parent(4, "lsp", "$/progress", "rust-analyzer", None, None),
            // Segment 2 child
            make_message_with_id_parent(
                5,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(500),
            ),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);

        // Expand segment 1 only (parent index = 0 for First segment).
        panel.expanded.insert(0);
        let flat = panel.flat_lines();

        // Segment 1 ScopeHeader + 2 ScopeChildren + interruption + Segment 2 ScopeHeader (collapsed)
        assert_eq!(
            flat.len(),
            5,
            "segment 1 expanded, segment 2 collapsed: {flat:?}"
        );
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { .. }),
            "first should be segment 1 ScopeHeader"
        );
        assert!(
            matches!(flat[1], FlatLine::ScopeChild { .. }),
            "second should be ScopeChild"
        );
        assert!(
            matches!(flat[2], FlatLine::ScopeChild { .. }),
            "third should be ScopeChild"
        );
        // flat[3] is the interruption (single or collapsed)
        assert!(
            matches!(flat[4], FlatLine::ScopeHeader { .. }),
            "fifth should be segment 2 ScopeHeader (collapsed)"
        );
    }

    #[test]
    fn test_segmented_scope_filter_hides_interruption() {
        // Filter out the interrupting entry. The pipeline runs scope
        // collapse before filtering, so the two segments remain — but
        // the interruption is hidden from the flat line output.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            {
                let mut m = make_message_with_id_parent(
                    1,
                    "mcp",
                    "tools/call",
                    "catenary",
                    Some(500),
                    None,
                );
                m.payload = serde_json::json!({"params": {"name": "grep"}});
                m
            },
            // Segment 1 child
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            // Root interruption — progress with a distinct method for filtering
            make_message_with_id_parent(3, "lsp", "$/progress", "rust-analyzer", None, None),
            // Segment 2 child
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(500),
            ),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);

        // Without filter: 2 segments + 1 interruption = 3 flat lines.
        let flat = panel.flat_lines();
        assert_eq!(
            flat.len(),
            3,
            "unfiltered: 2 segments + 1 interruption: {flat:?}"
        );

        // Filter to only show "grep" — matches scope headers but not the
        // progress interruption. Segments remain separate (scope collapse
        // runs before filtering) but the interruption is hidden.
        panel.filter_pattern = Some("grep".to_string());
        let flat = panel.flat_lines();
        assert_eq!(
            flat.len(),
            2,
            "filtered: 2 segments, interruption hidden: {flat:?}"
        );
    }

    #[test]
    fn test_segmented_scope_plain_format() {
        // Verify plain text output includes the ellipsis convention.
        let make_tool_call = || {
            let mut m =
                make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None);
            m.payload = serde_json::json!({"params": {"name": "grep"}});
            m
        };

        let parent_entry = DisplayEntry::Single {
            index: 0,
            parent_id: None,
        };
        let messages = vec![make_tool_call()];

        let plain_first = format_scope_plain(&parent_entry, 3, SegmentPosition::First, &messages);
        assert!(
            plain_first.contains("grep\u{2026}"),
            "First plain should contain grep…: {plain_first}"
        );

        let plain_middle = format_scope_plain(&parent_entry, 2, SegmentPosition::Middle, &messages);
        assert!(
            plain_middle.contains("\u{2026}grep\u{2026}"),
            "Middle plain should contain …grep…: {plain_middle}"
        );

        let plain_last = format_scope_plain(&parent_entry, 1, SegmentPosition::Last, &messages);
        assert!(
            plain_last.contains("\u{2026}grep"),
            "Last plain should contain …grep: {plain_last}"
        );

        let plain_only = format_scope_plain(&parent_entry, 5, SegmentPosition::Only, &messages);
        assert!(
            !plain_only.contains('\u{2026}'),
            "Only plain should not contain ellipsis: {plain_only}"
        );
    }

    fn make_message_with_id_parent_payload(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        payload: serde_json::Value,
    ) -> SessionMessage {
        test_support::message_with_ids_payload(
            id, r#type, method, server, request_id, parent_id, payload,
        )
    }

    #[test]
    fn test_separator_toggle_collapses_parent() {
        // Cursor on a separator line (inside ScopeChild). Toggle expansion.
        // Verify parent scope collapses and cursor moves to the ScopeHeader.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent_payload(
                1,
                "mcp",
                "tools/call",
                "catenary",
                Some(500),
                None,
                serde_json::json!({"params": {"name": "grep"}}),
            ),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
        ];
        let fm_count = frontmatter_lines(&messages[0], &theme).len();
        assert!(fm_count > 0, "parent should have frontmatter");

        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Expansion key = parent index = 0
        panel.expanded.insert(0);

        // Find the separator line index
        let flat = panel.flat_lines();
        let sep_idx = flat
            .iter()
            .position(|fl| {
                matches!(
                    fl,
                    FlatLine::ScopeChild { inner, .. }
                        if matches!(inner.as_ref(), FlatLine::Separator)
                )
            })
            .expect("should have separator");

        panel.cursor = sep_idx;
        panel.toggle_expansion();

        // Scope should be collapsed
        assert!(
            !panel.expanded.contains(&0),
            "scope should be collapsed after toggling on separator"
        );
        // Cursor should be on the ScopeHeader
        assert_eq!(
            panel.cursor, 0,
            "cursor should move to ScopeHeader after separator toggle"
        );
    }

    #[test]
    fn test_panel_title_alive_style() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("abcd1234".to_string(), &theme, &icons);
        assert!(panel.alive);

        let title = build_title(&panel, 60);
        // Find the session ID span.
        let id_span = title
            .spans
            .iter()
            .find(|s| s.content.contains("abcd1234"))
            .expect("session ID span");
        assert_eq!(id_span.style, theme.session_active);
    }

    #[test]
    fn test_panel_title_dead_style() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abcd1234".to_string(), &theme, &icons);
        panel.alive = false;

        let title = build_title(&panel, 60);
        let id_span = title
            .spans
            .iter()
            .find(|s| s.content.contains("abcd1234"))
            .expect("session ID span");
        assert_eq!(id_span.style, theme.session_dead);
    }

    #[test]
    fn test_panel_title_degradation_drops_ls() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abcd1234".to_string(), &theme, &icons);
        panel.language_servers = vec!["rust-analyzer".to_string()];

        // Wide: LS info visible.
        let wide = build_title(&panel, 60);
        let wide_text: String = wide.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            wide_text.contains("rust-analyzer"),
            "wide title should contain LS name: {wide_text}"
        );

        // Narrow: LS info dropped.
        let narrow = build_title(&panel, 20);
        let narrow_text: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !narrow_text.contains("rust-analyzer"),
            "narrow title should not contain LS name: {narrow_text}"
        );
        assert!(
            narrow_text.contains("abcd1234"),
            "narrow title should still contain session ID: {narrow_text}"
        );
    }

    #[test]
    fn test_panel_title_degradation_truncates_id() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("abcd1234".to_string(), &theme, &icons);

        // Very narrow: session ID truncated, "Events" dropped.
        let title = build_title(&panel, 10);
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("Events"),
            "very narrow title should drop Events: {text}"
        );
        assert!(
            text.contains("abcd"),
            "very narrow title should contain ID prefix: {text}"
        );
    }

    #[test]
    fn test_panel_title_alive_style_survives_degradation() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("abcd1234".to_string(), &theme, &icons);

        // Even at narrow width, the session ID span has alive style.
        let title = build_title(&panel, 8);
        let id_span = title
            .spans
            .iter()
            .find(|s| s.style == theme.session_active)
            .expect("should have an alive-styled span");
        assert!(
            !id_span.content.is_empty(),
            "alive-styled span should have content"
        );
    }
}
