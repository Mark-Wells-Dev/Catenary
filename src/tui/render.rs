// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Pure renderers: rows → styled line groups, the contextual detail pane, and
//! the header/footer strips.
//!
//! Every function here is a pure function of the model — no policy, no state
//! mutation. The severity ladder is rendered with a glyph *and* a color, so the
//! cursor and problem tiers read on a monochrome terminal too (the accessibility
//! ruling: a glyph column alongside color).

use chrono::{DateTime, Utc};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::{Config, ConfigLayer};
use crate::health::Severity;
use crate::state_snapshot::{ServerEntry, SessionEntry, SessionStatus, Snapshot};

use super::action::{ActionState, InstallState, PendingRestart};
use super::findings::{OwnedFinding, Owner};
use super::format::{
    elapsed_at, format_elapsed_secs, format_freshness, seconds_between, truncate_to_width,
};
use super::icons::{IconSet, basename};
use super::model::{ClientRow, EntityKey, ProblemRow, RootRow, Row, Verdict};
use super::theme::Theme;

/// A session is judged stale (degrade to "last seen Nm") after this idle span.
const SESSION_STALE_SECS: i64 = 120;

/// Snapshot staleness (color the header staleness clock) after this span.
const SNAPSHOT_STALE_SECS: i64 = 30;

/// The color+modifier for a severity tier.
#[must_use]
pub const fn severity_style(theme: &Theme, sev: Severity) -> Style {
    match sev {
        Severity::Fatal => theme.error.add_modifier(Modifier::BOLD),
        Severity::Error => theme.error,
        Severity::Warning => theme.warning,
        Severity::Suggestion => theme.accent,
        Severity::Ok => theme.success,
        Severity::Info => theme.muted,
    }
}

/// The glyph for a severity tier (the color-independent signal).
#[must_use]
pub const fn severity_glyph(sev: Severity) -> &'static str {
    match sev {
        Severity::Fatal | Severity::Error => "✗",
        Severity::Warning => "⚠",
        Severity::Suggestion => "○",
        Severity::Ok => "✓",
        Severity::Info => "·",
    }
}

/// Leading gutter (2 cols, the cursor column) + `depth` indent.
fn indent(depth: u8) -> String {
    " ".repeat(2 + depth as usize * 2)
}

/// A short, display-friendly form of an opaque scope id.
fn short_id(id: &str) -> String {
    if id.chars().count() <= 18 {
        id.to_string()
    } else {
        let tail: String = id
            .chars()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

/// Compact contributor label from a root's sources (`[hook mcp:27 worktree:1]`).
///
/// The `mcp` class carries a **session tag** when a single MCP connection holds
/// the root (tui-rework 14, item 6b): `[mcp:27]` names the connection so the two
/// roots one session mounts (e.g. `Catenary` + `CatenaryInternal`) share a tag
/// and stay distinguishable from another session's pair — replacing the bare
/// `[mcp:1]` count that collapsed session identity. When several connections
/// hold the root, no single tag disambiguates, so the count returns (`mcp:2`).
#[must_use]
pub fn contributor_label(sources: &[String]) -> String {
    let (mut hook, mut worktree, mut ephemeral, mut other) = (false, 0, false, 0);
    let mut mcp_tags: Vec<&str> = Vec::new();
    for s in sources {
        if s == "hook" {
            hook = true;
        } else if let Some(tag) = s.strip_prefix("mcp:") {
            mcp_tags.push(tag);
        } else if s.starts_with("worktree:") {
            worktree += 1;
        } else if s.starts_with("ephemeral:") {
            ephemeral = true;
        } else {
            other += 1;
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if hook {
        parts.push("hook".to_string());
    }
    match mcp_tags.as_slice() {
        [] => {}
        [tag] => parts.push(format!("mcp:{}", session_tag(tag))),
        tags => parts.push(format!("mcp:{}", tags.len())),
    }
    if worktree > 0 {
        parts.push(format!("worktree:{worktree}"));
    }
    if ephemeral {
        parts.push("ephemeral".to_string());
    }
    if other > 0 {
        parts.push(format!("+{other}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("[{}]", parts.join(" "))
    }
}

/// A short, disambiguating tag for a contributor connection id — the raw id when
/// it is already short (a small `mcp:{fd}`), else its `…`-elided tail so a long
/// opaque id stays compact (tui-rework 14, item 6b).
fn session_tag(id: &str) -> String {
    if id.chars().count() <= 6 {
        id.to_string()
    } else {
        let tail: String = id
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

/// Style + label for a server lifecycle state.
fn state_style(state: &str, busy_count: Option<u32>, theme: &Theme) -> (String, Style) {
    match state {
        "healthy" => ("healthy".to_string(), theme.success),
        "busy" => (
            busy_count.map_or_else(|| "busy".to_string(), |n| format!("busy({n})")),
            theme.accent,
        ),
        "initializing" => ("initializing".to_string(), theme.warning),
        "probing" => ("probing".to_string(), theme.warning),
        "failed" => ("failed".to_string(), theme.error),
        "dead" => ("dead".to_string(), theme.muted),
        other => (other.to_string(), theme.text),
    }
}

/// Capability-aware session status cell — renders only what the snapshot feeds.
///
/// The schema-2 snapshot carries Catenary's derived activity
/// ([`SessionStatus`]) plus `last_seen`; it does **not** carry a per-host
/// "stopped" / "permission-blocked" flag, so — honoring "no fabricated
/// statuses" — a stale session degrades to `last seen Nm` for every host, and a
/// fresh one shows its activity. (Claude subagents render as sub-rows; that is
/// the one host capability the snapshot distinguishes.)
fn session_status_cell(entry: &SessionEntry, theme: &Theme, now: DateTime<Utc>) -> (String, Style) {
    if let Some(secs) = seconds_between(&entry.last_seen, now)
        && secs > SESSION_STALE_SECS
    {
        return (
            format!("last seen {}", elapsed_at(&entry.last_seen, now)),
            theme.muted,
        );
    }
    status_cell(entry.status, theme)
}

/// The label + style for a derived [`SessionStatus`], with no staleness gate.
///
/// The shared status vocabulary for both a session row and a subagent sub-row
/// (tui-rework 14, item 3): `editing` (gate armed), `working` (gate paid, still
/// editing), `diagnostics` (a run in flight), and `idle`. An unknown future
/// status a stale reader cannot name renders as quiet `idle` — never a false
/// `editing`.
fn status_cell(status: SessionStatus, theme: &Theme) -> (String, Style) {
    match status {
        SessionStatus::Editing => ("editing".to_string(), theme.session_active),
        SessionStatus::Working => ("working".to_string(), theme.success),
        SessionStatus::Diagnostics => ("diagnostics".to_string(), theme.accent),
        SessionStatus::Idle | SessionStatus::Unknown => ("idle".to_string(), theme.session_meta),
    }
}

/// Render one tree row into a single styled line.
///
/// `now` is the injected render clock: every duration on the row is computed
/// against it so the row is byte-identical between refreshes within a
/// quantization bucket (tui-rework 11, item 4).
#[must_use]
pub fn tree_line(
    row: &Row,
    width: usize,
    theme: &Theme,
    icons: &IconSet,
    now: DateTime<Utc>,
) -> Line<'static> {
    match row {
        Row::Root(r) => root_line(r, width, theme, icons),
        Row::Server { entry, subroot } => {
            server_line(entry, subroot.as_deref(), width, theme, icons, now)
        }
        Row::InlineFinding {
            severity,
            message,
            depth,
        } => finding_line(*severity, message, *depth, width, theme),
        Row::DormantToggle { count, expanded } => {
            let glyph = if *expanded {
                &icons.workspace_open
            } else {
                &icons.workspace_closed
            };
            Line::from(vec![Span::styled(
                format!("  {glyph} {count} dormant servers (configured, not running)"),
                theme.muted,
            )])
        }
        Row::Dormant(name) => Line::from(vec![Span::styled(
            format!("{}{name}", indent(1)),
            theme.muted,
        )]),
        Row::Client(c) => client_line(c, width, theme, icons),
        Row::Session(s) => session_line(s, width, theme, now),
        Row::Subagent { subagent, .. } => subagent_line(subagent, width, theme, now),
    }
}

/// The wrapped, full-text form of a tree row for the row under the cursor.
///
/// The same content as [`tree_line`] (tui-rework 12, item 1) but reflowed across
/// as many lines as its full text needs rather than truncated to one line with
/// `…`. Continuation lines indent under the text column (2 cols, the caret
/// gutter); the caller draws the caret on the first line only. Right-column
/// status/time (up-count, time-in-state) follows the left content inline so it
/// survives the wrap.
#[must_use]
pub fn tree_line_wrapped(
    row: &Row,
    width: usize,
    theme: &Theme,
    icons: &IconSet,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let spans = match row {
        Row::Root(r) => join_lr(root_parts(r, theme, icons)),
        Row::Server { entry, subroot } => {
            join_lr(server_parts(entry, subroot.as_deref(), theme, icons, now))
        }
        Row::Client(c) => join_lr(client_parts(c, theme, icons)),
        Row::Session(s) => join_lr(session_parts(s, theme, now)),
        Row::InlineFinding {
            severity,
            message,
            depth,
        } => vec![
            Span::styled(
                format!("{}{} ", indent(*depth), severity_glyph(*severity)),
                severity_style(theme, *severity),
            ),
            Span::styled(message.clone(), severity_style(theme, *severity)),
        ],
        // Rows with fixed, already-compact text stay one line even when selected.
        Row::DormantToggle { .. } | Row::Dormant(_) | Row::Subagent { .. } => {
            return vec![tree_line(row, width, theme, icons, now)];
        }
    };
    // Only wrap when the full text overflows; a row that already fits keeps its
    // normal (right-justified) single line — selection adds bold + a caret, not a
    // layout change (tui-rework 12, item 1: compact by default).
    if super::format::spans_width(&spans) <= width {
        return vec![tree_line(row, width, theme, icons, now)];
    }
    super::format::wrap_line(&spans, width, 2)
}

/// Join a left/right span pair into a single inline span sequence separated by a
/// space, so the right column (status/time) wraps alongside the left rather than
/// being right-flushed. Used only by the wrapped selected-row path.
fn join_lr(parts: (Vec<Span<'static>>, Vec<Span<'static>>)) -> Vec<Span<'static>> {
    let (mut left, right) = parts;
    if !right.is_empty() {
        left.push(Span::raw(" ".to_string()));
        left.extend(right);
    }
    left
}

/// Compact lifetime badge for a root, keyed on the lifetime its sources imply
/// (tui-rework 14, item 6c): `[pin]` for a hook-held (pinned / cwd-tracked)
/// root, `[worktree]` for a worktree-scoped mount, `[activity]` for an ephemeral
/// activity mount. Empty when only an MCP connection holds it — its lifetime is
/// already conveyed by the `[mcp:…]` session tag. Pin outranks the others (a
/// pinned root does not idle-expire regardless of what else holds it).
fn lifetime_badge(sources: &[String], ephemeral: bool) -> &'static str {
    if sources.iter().any(|s| s == "hook") {
        "[pin]"
    } else if sources.iter().any(|s| s.starts_with("worktree:")) {
        "[worktree]"
    } else if ephemeral {
        "[activity]"
    } else {
        ""
    }
}

fn root_parts(
    r: &RootRow,
    theme: &Theme,
    icons: &IconSet,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let glyph = if r.expanded {
        &icons.workspace_open
    } else {
        &icons.workspace_closed
    };
    // A companion root nests: an extra indent step and a `↳` marker before the
    // folder glyph, so it reads as a child of its primary (item 6a).
    let (indent, marker) = if r.companion_of.is_some() {
        ("    ".to_string(), "↳ ".to_string())
    } else {
        ("  ".to_string(), String::new())
    };
    let mut left = vec![
        Span::raw(indent),
        Span::styled(marker, theme.muted),
        Span::styled(format!("{glyph} "), theme.accent),
        Span::styled(basename(&r.path).to_string(), theme.text),
    ];
    let label = contributor_label(&r.sources);
    if !label.is_empty() {
        left.push(Span::styled(format!("  {label}"), theme.muted));
    }
    let badge = lifetime_badge(&r.sources, r.ephemeral);
    if !badge.is_empty() {
        left.push(Span::styled(format!("  {badge}"), theme.timestamp));
    }
    if r.ephemeral {
        let idle = r.idle_remaining_secs.map_or_else(
            || "idle".to_string(),
            |s| {
                format!(
                    "idle {}",
                    format_elapsed_secs(i64::try_from(s).unwrap_or(i64::MAX))
                )
            },
        );
        left.push(Span::styled(format!("  {idle}"), theme.timestamp));
    }
    let mut right = Vec::new();
    if let Some(w) = r.worst {
        right.push(Span::styled(
            format!("{} ", severity_glyph(w)),
            severity_style(theme, w),
        ));
    }
    right.push(Span::styled(
        format!("{}/{} up", r.up, r.total),
        theme.muted,
    ));
    (left, right)
}

fn root_line(r: &RootRow, width: usize, theme: &Theme, icons: &IconSet) -> Line<'static> {
    let (left, right) = root_parts(r, theme, icons);
    super::format::justify(left, right, width)
}

fn server_parts(
    e: &ServerEntry,
    subroot: Option<&str>,
    theme: &Theme,
    icons: &IconSet,
    now: DateTime<Utc>,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let (label, style) = state_style(&e.state, e.busy_count, theme);
    let up = super::model::is_up(&e.state);
    let dot = if up {
        icons.ls_active.clone()
    } else {
        icons.ls_inactive.clone()
    };
    let mut left = vec![
        Span::raw(indent(1)),
        Span::styled(format!("{dot} "), style),
        Span::styled(e.server.clone(), theme.text),
    ];
    if !e.language.is_empty() {
        left.push(Span::styled(format!("  {}", e.language), theme.muted));
    }
    // A server anchored on a subtree of its tracked root shows that subpath
    // relative to the root — the fixture subdirectory it walked to, no longer a
    // phantom top-level peer root (tui-rework 14, item 5).
    if let Some(rel) = subroot {
        left.push(Span::styled(format!("  ./{rel}"), theme.timestamp));
    }
    let mut right = Vec::new();
    if e.respawns > 0 {
        let death = e
            .last_died_at
            .as_deref()
            .map(|d| format!(" died {}", elapsed_at(d, now)))
            .unwrap_or_default();
        right.push(Span::styled(
            format!("↻{}{death} ", e.respawns),
            theme.warning,
        ));
    }
    if e.degraded_reason.is_some() {
        right.push(Span::styled("⚠ ".to_string(), theme.warning));
    }
    let tis = elapsed_at(&e.state_since, now);
    if !tis.is_empty() {
        right.push(Span::styled(format!("{tis} "), theme.timestamp));
    }
    right.push(Span::styled(label, style));
    (left, right)
}

fn server_line(
    e: &ServerEntry,
    subroot: Option<&str>,
    width: usize,
    theme: &Theme,
    icons: &IconSet,
    now: DateTime<Utc>,
) -> Line<'static> {
    let (left, right) = server_parts(e, subroot, theme, icons, now);
    super::format::justify(left, right, width)
}

fn finding_line(
    severity: Severity,
    message: &str,
    depth: u8,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let prefix = format!("{}{} ", indent(depth), severity_glyph(severity));
    let avail = width.saturating_sub(prefix.chars().count());
    Line::from(vec![
        Span::styled(prefix, severity_style(theme, severity)),
        Span::styled(
            truncate_to_width(message, avail),
            severity_style(theme, severity),
        ),
    ])
}

fn client_parts(
    c: &ClientRow,
    theme: &Theme,
    icons: &IconSet,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let glyph = if c.expanded {
        &icons.workspace_open
    } else {
        &icons.workspace_closed
    };
    let mut left = vec![
        Span::raw("  ".to_string()),
        Span::styled(format!("{glyph} "), theme.accent),
        Span::styled(c.name.clone(), theme.text),
        Span::styled(format!("  ({} sessions)", c.sessions), theme.muted),
    ];
    if c.issues > 0 {
        left.push(Span::styled(
            format!(
                "  ({} issue{})",
                c.issues,
                if c.issues == 1 { "" } else { "s" }
            ),
            c.worst.map_or(theme.muted, |w| severity_style(theme, w)),
        ));
    }
    let right = c.worst.map_or_else(Vec::new, |w| {
        vec![Span::styled(
            severity_glyph(w).to_string(),
            severity_style(theme, w),
        )]
    });
    (left, right)
}

fn client_line(c: &ClientRow, width: usize, theme: &Theme, icons: &IconSet) -> Line<'static> {
    let (left, right) = client_parts(c, theme, icons);
    super::format::justify(left, right, width)
}

/// Whether the session is idle-stale: past the staleness gate for its
/// `last_seen`.
fn session_is_stale(entry: &SessionEntry, now: DateTime<Utc>) -> bool {
    seconds_between(&entry.last_seen, now).is_some_and(|secs| secs > SESSION_STALE_SECS)
}

/// The `last_action` summary to render on a session row/detail, scoping the
/// diagnostics summary to its batch (tui-rework 14, item 2).
///
/// A `diagnostics: N errors, M warnings` line is shown only while the batch that
/// produced it is still current — the session is actively editing/working or
/// running diagnostics and not idle-stale. Once the batch closes (status back to
/// `idle`/`unknown`) or the session goes stale, the line is dropped so a
/// zero-count summary does not loiter (the idle-board law). A non-diagnostics
/// action (`edited …`) is always shown — it is not batch-scoped.
fn current_action_summary(entry: &SessionEntry, now: DateTime<Utc>) -> Option<&str> {
    let action = entry.last_action.as_ref()?;
    let is_diagnostics = action.summary.starts_with("diagnostics:");
    if !is_diagnostics {
        return Some(action.summary.as_str());
    }
    let batch_current = matches!(
        entry.status,
        SessionStatus::Editing | SessionStatus::Working | SessionStatus::Diagnostics
    ) && !session_is_stale(entry, now);
    batch_current.then_some(action.summary.as_str())
}

fn session_parts(
    s: &SessionEntry,
    theme: &Theme,
    now: DateTime<Utc>,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let (cell, cell_style) = session_status_cell(s, theme, now);
    let mut left = vec![
        Span::raw(indent(1)),
        Span::styled(short_id(&s.id), theme.text),
    ];
    if let Some(summary) = current_action_summary(s, now) {
        left.push(Span::styled(format!("  {summary}"), theme.muted));
    }
    let right = vec![Span::styled(cell, cell_style)];
    (left, right)
}

fn session_line(
    s: &SessionEntry,
    width: usize,
    theme: &Theme,
    now: DateTime<Utc>,
) -> Line<'static> {
    let (left, right) = session_parts(s, theme, now);
    super::format::justify(left, right, width)
}

/// A subagent sub-row's left/right span pair: `⤷ <id>  up <age>` on the left,
/// the capability-aware status cell on the right (tui-rework 14, item 3).
fn subagent_parts(
    s: &crate::state_snapshot::Subagent,
    theme: &Theme,
    now: DateTime<Utc>,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let left = vec![Span::styled(
        format!(
            "{}⤷ {}  up {}",
            indent(2),
            short_id(&s.id),
            elapsed_at(&s.started_at, now)
        ),
        theme.session_meta,
    )];
    let (cell, cell_style) = status_cell(s.status, theme);
    let right = vec![Span::styled(cell, cell_style)];
    (left, right)
}

fn subagent_line(
    s: &crate::state_snapshot::Subagent,
    width: usize,
    theme: &Theme,
    now: DateTime<Utc>,
) -> Line<'static> {
    let (left, right) = subagent_parts(s, theme, now);
    super::format::justify(left, right, width)
}

// ── Problems pane ────────────────────────────────────────────────────

/// Render the problems pane as entry groups (one or two lines each).
///
/// Each entry is the labelled finding, then its fix-it indented. A suggestion
/// tail renders with a dim header so it can never be mistaken for a problem. The
/// entry at `selected` (an entry index, matching what the list highlights) wraps
/// its finding message to the full text across as many lines as it needs rather
/// than truncating it to one line with `…` (tui-rework 12, item 1); every other
/// entry stays compact. Continuation lines indent under the message column.
#[must_use]
pub fn problem_entries(
    rows: &[ProblemRow],
    width: usize,
    theme: &Theme,
    selected: Option<usize>,
) -> Vec<Vec<Line<'static>>> {
    let mut out: Vec<Vec<Line<'static>>> = Vec::new();
    let mut suggestion_header_emitted = false;
    for r in rows {
        if r.is_suggestion && !suggestion_header_emitted {
            out.push(vec![Line::from(vec![Span::styled(
                "  suggestions".to_string(),
                theme.muted,
            )])]);
            suggestion_header_emitted = true;
        }
        let style = severity_style(theme, r.severity);
        let label = format!("  {}: ", r.severity.label());
        let label_w = label.chars().count();
        let is_selected = selected == Some(out.len());
        let mut lines = if is_selected {
            // Full message, wrapped under the message column (item 1).
            let head = vec![
                Span::styled(label, style),
                Span::styled(r.message.clone(), style),
            ];
            super::format::wrap_line(&head, width, label_w)
        } else {
            let head_avail = width.saturating_sub(label_w);
            vec![Line::from(vec![
                Span::styled(label, style),
                Span::styled(truncate_to_width(&r.message, head_avail), style),
            ])]
        };
        if let Some(fix) = &r.fix_it {
            for l in fix.lines() {
                lines.push(Line::from(vec![Span::styled(
                    format!("    {}", truncate_to_width(l, width.saturating_sub(4))),
                    theme.muted,
                )]));
            }
        }
        out.push(lines);
    }
    out
}

/// Render the pending-restart markers appended to the problems pane.
///
/// An applied mutation is written but not yet live (config changes need a daemon
/// restart), so each stays listed here — marked, never silently gone — until the
/// daemon comes back on the new config.
#[must_use]
pub fn pending_restart_entries(
    pending: &[PendingRestart],
    width: usize,
    theme: &Theme,
) -> Vec<Vec<Line<'static>>> {
    if pending.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Vec<Line<'static>>> = vec![vec![Line::from(vec![Span::styled(
        format!("  ⟳ {} pending daemon restart", pending.len()),
        theme.warning,
    )])]];
    for p in pending {
        out.push(vec![Line::from(vec![Span::styled(
            format!(
                "    {}",
                truncate_to_width(&p.summary, width.saturating_sub(4))
            ),
            theme.muted,
        )])]);
    }
    out
}

/// Render the guided-mutation consent overlay: what will be written (key, value,
/// target file), the layer choice when both apply, and the confirm/cancel hint.
///
/// The overlay shows exactly what a confirm writes — no silent mutation ever.
#[must_use]
pub fn action_overlay_lines(state: &ActionState, theme: &Theme) -> Vec<Line<'static>> {
    let m = state.mutation();
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "  Apply this change?".to_string(),
            theme.title,
        )]),
        Line::from(""),
        kv("key", m.key_label(), theme),
        kv("value", state.preview_value(), theme),
    ];
    let target = state
        .current_layer()
        .map_or_else(|| "—".to_string(), ConfigLayer::label);
    lines.push(kv("target", target, theme));
    if state.candidate_count() > 1 {
        lines.push(Line::from(vec![Span::styled(
            "  Tab: switch user / project layer".to_string(),
            theme.muted,
        )]));
    }
    if state.takes_value() {
        lines.push(Line::from(vec![Span::styled(
            "  type to edit · Backspace deletes".to_string(),
            theme.muted,
        )]));
    }
    if let Some(err) = &state.error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  ✗ {err}"),
            theme.error,
        )]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Enter".to_string(), theme.hint_key),
        Span::styled(" apply   ".to_string(), theme.hint_label),
        Span::styled("Esc".to_string(), theme.hint_key),
        Span::styled(" cancel".to_string(), theme.hint_label),
    ]));
    lines
}

/// Render the guided-install consent overlay: the pinned command/artifact, the
/// verification tier, and exactly what runs — or, once run, the streamed outcome.
///
/// The overlay shows the exact pinned artifact a confirm fetches, verifies, and
/// installs; no unpinned or unverified install is ever presented.
#[must_use]
pub fn install_overlay_lines(state: &InstallState, theme: &Theme) -> Vec<Line<'static>> {
    // Once the install has run, show the streamed outcome in place of the preview.
    if let Some(outcome) = state.outcome() {
        let head = if outcome.success {
            Span::styled(format!("  Installed {} ✓", state.server()), theme.success)
        } else {
            Span::styled(
                format!("  Install {} failed ✗", state.server()),
                theme.error,
            )
        };
        let mut lines = vec![Line::from(head), Line::from("")];
        for entry in &outcome.log {
            lines.push(Line::from(vec![Span::styled(
                format!("  {entry}"),
                theme.muted,
            )]));
        }
        lines.push(Line::from(""));
        lines.push(dismiss_hint(theme));
        return lines;
    }

    let mut lines = vec![
        title(format!("  Install {}?", state.server()), theme),
        Line::from(""),
    ];
    match state.plan() {
        Err(reason) => {
            lines.push(Line::from(vec![Span::styled(
                format!("  ✗ {reason}"),
                theme.error,
            )]));
            lines.push(Line::from(""));
            lines.push(dismiss_hint(theme));
        }
        Ok(plan) => {
            lines.push(kv(
                "via",
                format!("{} · {}", plan.ecosystem().as_str(), plan.verify_summary()),
                theme,
            ));
            lines.push(kv(
                "package",
                format!("{}@{}", plan.package(), plan.version()),
                theme,
            ));
            if let Some(url) = plan.fetch_url() {
                lines.push(kv("fetch", url.to_string(), theme));
            }
            lines.push(kv("runs", plan.display_command(), theme));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  Enter".to_string(), theme.hint_key),
                Span::styled(" install   ".to_string(), theme.hint_label),
                Span::styled("Esc".to_string(), theme.hint_key),
                Span::styled(" cancel".to_string(), theme.hint_label),
            ]));
        }
    }
    lines
}

/// The `Esc close` hint line shared by the install overlay's terminal states.
fn dismiss_hint(theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("  Esc".to_string(), theme.hint_key),
        Span::styled(" close".to_string(), theme.hint_label),
    ])
}

// ── Header / footer strips ───────────────────────────────────────────

/// The one-line verdict span sequence (`● working` / `✗ N problems`), with a
/// suggestion tail when suggestions exist.
#[must_use]
pub fn verdict_spans(verdict: Verdict, daemon_up: bool, theme: &Theme) -> Vec<Span<'static>> {
    if !daemon_up {
        return vec![Span::styled("◌ daemon down".to_string(), theme.muted)];
    }
    let mut spans = if verdict.is_working() {
        vec![Span::styled("● working".to_string(), theme.success)]
    } else {
        let n = verdict.problems();
        vec![Span::styled(
            format!("✗ {n} problem{}", if n == 1 { "" } else { "s" }),
            theme.error.add_modifier(Modifier::BOLD),
        )]
    };
    if verdict.suggestion > 0 {
        spans.push(Span::styled(
            format!(
                "  · {} suggestion{}",
                verdict.suggestion,
                if verdict.suggestion == 1 { "" } else { "s" }
            ),
            theme.accent,
        ));
    }
    spans
}

/// The daemon-status spans for the footer: pid, version (+ skew), and snapshot
/// freshness (tui-rework 09, item 2 — moved off the retired header strip).
///
/// Empty when the daemon is down (no snapshot generated). The version reads from
/// the single [`BINARY_VERSION`](crate::health::skew::BINARY_VERSION) source, so
/// it agrees with `catenary version` and the skew finding — a non-tag build is
/// never falsely flagged (item 1).
#[must_use]
pub fn daemon_status_spans(
    snapshot: &Snapshot,
    theme: &Theme,
    now: DateTime<Utc>,
) -> Vec<Span<'static>> {
    if snapshot.daemon.generated_at.is_empty() {
        return Vec::new();
    }
    let sep = || Span::styled(" · ".to_string(), theme.muted);
    let mut spans = vec![Span::styled(
        format!("daemon pid {}", snapshot.daemon.pid),
        theme.muted,
    )];

    // Version + skew.
    spans.push(sep());
    let binary = crate::health::skew::BINARY_VERSION;
    let dv = &snapshot.daemon.version;
    if dv.is_empty() || dv == binary {
        spans.push(Span::styled(format!("v{binary}"), theme.muted));
    } else {
        spans.push(Span::styled(format!("v{dv}"), theme.warning));
        spans.push(Span::styled(
            format!(" (binary v{binary} — skew)"),
            theme.warning,
        ));
    }

    // Snapshot freshness rides with the daemon identity (quantized — item 4).
    if let Some(secs) = seconds_between(&snapshot.daemon.generated_at, now) {
        spans.push(sep());
        let style = if secs > SNAPSHOT_STALE_SECS {
            theme.warning
        } else {
            theme.muted
        };
        spans.push(Span::styled(
            format!("updated {}", format_freshness(secs)),
            style,
        ));
    }
    spans
}

/// The slimmed footer status bar (tui-rework 11, item 2).
///
/// The sole `? keys` discovery hint packs left; the daemon status (pid ·
/// version + skew · freshness) flushes right. The per-key hints left the footer
/// — the `?` panel carries the full key list. The whole line is bounded to
/// `width` and truncated with `…` (item 1) so the freshness never clips raw at
/// the terminal edge; the `? keys` hint is kept (the daemon status yields its
/// tail first).
#[must_use]
pub fn footer_line(
    snapshot: &Snapshot,
    width: usize,
    theme: &Theme,
    now: DateTime<Utc>,
) -> Line<'static> {
    let left = vec![
        Span::styled(" ? ".to_string(), theme.hint_key),
        Span::styled("keys".to_string(), theme.hint_label),
    ];
    let left_w = super::format::spans_width(&left);
    let right = daemon_status_spans(snapshot, theme, now);
    if right.is_empty() {
        return super::format::bound_line(&Line::from(left), width);
    }
    // Keep `? keys` — the sole discovery hint — and let the daemon status yield
    // its tail (freshness first) so the line never clips raw (items 1 & 2).
    let right = super::format::truncate_spans(right, width.saturating_sub(left_w + 1));
    let right_w = super::format::spans_width(&right);
    let gap = width.saturating_sub(left_w + right_w);
    let mut spans = left;
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.extend(right);
    super::format::bound_line(&Line::from(spans), width)
}

// ── Detail pane ──────────────────────────────────────────────────────

/// Render the contextual detail pane for the cursored entity.
///
/// `now` is the injected render clock (tui-rework 11, item 4): the detail pane
/// keeps finer wording than the board but still quantizes so it does not tick
/// every second.
#[must_use]
pub fn detail_lines(
    entity: Option<&EntityKey>,
    snapshot: &Snapshot,
    config: Option<&Config>,
    findings: &[OwnedFinding],
    theme: &Theme,
    now: DateTime<Utc>,
    width: usize,
) -> Vec<Line<'static>> {
    match entity {
        None => vec![Line::from(vec![Span::styled(
            "  Select a node to see details.".to_string(),
            theme.muted,
        )])],
        Some(EntityKey::Root(path)) => root_detail(path, snapshot, theme),
        Some(EntityKey::Server { name, .. }) => {
            server_detail(name, snapshot, config, findings, theme, now)
        }
        Some(EntityKey::Client(name)) => client_detail(name, snapshot, findings, theme),
        Some(EntityKey::Session(id)) => session_detail(id, snapshot, theme, now, width),
        Some(EntityKey::Subagent { session, agent }) => {
            subagent_detail(session, agent, snapshot, theme, now, width)
        }
    }
}

fn title(text: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![Span::styled(text, theme.title)])
}

fn kv(k: &str, v: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {k}: "), theme.muted),
        Span::styled(v, theme.text),
    ])
}

fn root_detail(path: &str, snapshot: &Snapshot, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title(format!("Root  {path}"), theme)];
    if let Some(meta) = snapshot.roots.iter().find(|r| r.path == path) {
        let label = contributor_label(&meta.sources);
        lines.push(kv(
            "contributors",
            if label.is_empty() {
                "—".to_string()
            } else {
                label
            },
            theme,
        ));
        if meta.ephemeral {
            let idle = meta
                .idle_remaining_secs
                .map_or_else(|| "tracked".to_string(), |s| format!("{s}s remaining"));
            lines.push(kv("idle", idle, theme));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Routing (why a file is covered)".to_string(),
        theme.title,
    )]));
    // Include servers whose scope root is this root OR a subtree of it (a
    // fixture-subdir anchor) — the same nesting the tree renders (tui-rework 14,
    // item 5). A subtree anchor is named by its relative subpath so it stays
    // legibly attributed to this root, not hidden.
    let tracked: Vec<&str> = snapshot.roots.iter().map(|r| r.path.as_str()).collect();
    let mut servers: Vec<&ServerEntry> = snapshot
        .servers
        .iter()
        .filter(|s| super::model::server_nests_under(&s.scope_root, path, &tracked))
        .collect();
    servers.sort_by(|a, b| a.language.cmp(&b.language));
    if servers.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "    (no servers routed here)".to_string(),
            theme.muted,
        )]));
    }
    for s in servers {
        let (label, style) = state_style(&s.state, s.busy_count, theme);
        let sub = super::model::subroot_display(path, &s.scope_root);
        lines.push(Line::from(vec![
            Span::styled(
                format!("    {} → {}{}  ", s.language, s.server, sub),
                theme.text,
            ),
            Span::styled(label, style),
        ]));
    }
    lines
}

#[allow(
    clippy::too_many_lines,
    reason = "one cohesive detail: source, command, routing, findings + provenance, instances"
)]
fn server_detail(
    name: &str,
    snapshot: &Snapshot,
    config: Option<&Config>,
    findings: &[OwnedFinding],
    theme: &Theme,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let mut lines = vec![title(format!("Server  {name}"), theme)];

    // Provenance (coarse): a shipped default vs a user/project definition.
    let is_default = crate::config::default_server_names().contains(name);
    lines.push(kv(
        "source",
        if is_default {
            "default (shipped)".to_string()
        } else {
            "user / project config".to_string()
        },
        theme,
    ));

    if let Some(def) = config.and_then(|c| c.server.get(name)) {
        let cmd = if def.args.is_empty() {
            def.command.clone()
        } else {
            format!("{} {}", def.command, def.args.join(" "))
        };
        lines.push(kv("command", cmd, theme));
        // Name the resolved path (home-relativized), not just presence —
        // "where is it?" is the question the pane answers (ticket 11 item 5).
        let binary = crate::health::servers::resolve_binary(&def.command).map_or_else(
            || "NOT found on $PATH".to_string(),
            |path| crate::bridge::compress_home(&path),
        );
        lines.push(kv("binary", binary, theme));
    }

    // Languages routing to this server.
    if let Some(cfg) = config {
        let langs: Vec<&str> = cfg
            .language
            .iter()
            .filter(|(_, lc)| lc.servers().iter().any(|b| b.name == name))
            .map(|(l, _)| l.as_str())
            .collect();
        if !langs.is_empty() {
            lines.push(kv("routes for", langs.join(", "), theme));
        }
    }

    // Findings for this server: message, fix-it, then the routing provenance
    // (item 4) under the fix-it line — "why is this being probed at all?".
    let server_findings: Vec<&OwnedFinding> = findings
        .iter()
        .filter(|f| matches!(&f.owner, Owner::Server(s) if s == name))
        .collect();
    if !server_findings.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Findings".to_string(),
            theme.title,
        )]));
        for f in server_findings {
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "    {} {}",
                    severity_glyph(f.finding.severity),
                    f.finding.message
                ),
                severity_style(theme, f.finding.severity),
            )]));
            if let Some(fix) = &f.finding.fix_it {
                for l in fix.lines() {
                    lines.push(Line::from(vec![Span::styled(
                        format!("      {l}"),
                        theme.muted,
                    )]));
                }
            }
            if let Some(prov) = &f.finding.provenance {
                lines.push(Line::from(vec![Span::styled(
                    format!("      {prov}"),
                    theme.muted,
                )]));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Instances".to_string(),
        theme.title,
    )]));
    let instances: Vec<&ServerEntry> = snapshot
        .servers
        .iter()
        .filter(|s| s.server == name)
        .collect();
    if instances.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "    (no live instance)".to_string(),
            theme.muted,
        )]));
    }
    for s in instances {
        let (label, style) = state_style(&s.state, s.busy_count, theme);
        let root = if s.scope_root.is_empty() {
            s.scope_kind.clone()
        } else {
            basename(&s.scope_root).to_string()
        };
        let mut spans = vec![
            Span::styled(format!("    {root}  "), theme.text),
            Span::styled(format!("{label} "), style),
            Span::styled(elapsed_at(&s.state_since, now), theme.timestamp),
        ];
        if s.respawns > 0 {
            spans.push(Span::styled(format!("  ↻{}", s.respawns), theme.warning));
        }
        if let Some(d) = &s.last_died_at {
            spans.push(Span::styled(
                format!("  last death {} ago", elapsed_at(d, now)),
                theme.muted,
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn client_detail(
    name: &str,
    snapshot: &Snapshot,
    findings: &[OwnedFinding],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![title(format!("Client  {name}"), theme)];
    let sessions = snapshot
        .sessions
        .iter()
        .filter(|s| s.client.name == name)
        .count();
    lines.push(kv("live sessions", sessions.to_string(), theme));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Install health".to_string(),
        theme.title,
    )]));
    let mut any = false;
    for f in findings {
        if let Owner::Client(c) = &f.owner
            && c == name
        {
            any = true;
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "    {} {}",
                    severity_glyph(f.finding.severity),
                    f.finding.message
                ),
                severity_style(theme, f.finding.severity),
            )]));
            if let Some(fix) = &f.finding.fix_it {
                lines.push(Line::from(vec![Span::styled(
                    format!("      {fix}"),
                    theme.muted,
                )]));
            }
        }
    }
    if !any {
        lines.push(Line::from(vec![Span::styled(
            "    ✓ hooks and instructions up to date".to_string(),
            theme.success,
        )]));
    }
    lines
}

/// Emit an `id:`-labelled full identifier, wrapped to the pane width via 12's
/// `wrap_line` machinery so a long id is fully readable instead of truncated
/// (tui-rework 14, item 4). Short ids that fit stay a single line.
fn full_id_lines(label: &str, id: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let spans = vec![
        Span::styled(format!("  {label}: "), theme.muted),
        Span::styled(id.to_string(), theme.text),
    ];
    if super::format::spans_width(&spans) <= width {
        return vec![Line::from(spans)];
    }
    super::format::wrap_line(&spans, width, 2)
}

fn session_detail(
    id: &str,
    snapshot: &Snapshot,
    theme: &Theme,
    now: DateTime<Utc>,
    width: usize,
) -> Vec<Line<'static>> {
    let Some(s) = snapshot.sessions.iter().find(|s| s.id == id) else {
        let mut lines = vec![title("Session".to_string(), theme)];
        lines.extend(full_id_lines("id", id, theme, width));
        return lines;
    };
    let mut lines = vec![title("Session".to_string(), theme)];
    // The FULL session id, wrapped — the tree row keeps the short form.
    lines.extend(full_id_lines("id", id, theme, width));
    lines.push(kv("client", s.client.name.clone(), theme));
    let (cell, _) = session_status_cell(s, theme, now);
    lines.push(kv("status", cell, theme));
    lines.push(kv(
        "last seen",
        format!("{} ago", elapsed_at(&s.last_seen, now)),
        theme,
    ));
    if let (Some(summary), Some(a)) = (current_action_summary(s, now), s.last_action.as_ref()) {
        lines.push(kv(
            "last action",
            format!("{summary} ({} ago)", elapsed_at(&a.at, now)),
            theme,
        ));
    }
    if !s.roots.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  roots:".to_string(),
            theme.muted,
        )]));
        for r in &s.roots {
            lines.push(Line::from(vec![Span::styled(
                format!("    {r}"),
                theme.text,
            )]));
        }
    }
    if !s.subagents.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!("  subagents ({}):", s.subagents.len()),
            theme.title,
        )]));
        for sub in &s.subagents {
            let (cell, cell_style) = status_cell(sub.status, theme);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "    ⤷ {}  up {}  ",
                        short_id(&sub.id),
                        elapsed_at(&sub.started_at, now)
                    ),
                    theme.session_meta,
                ),
                Span::styled(cell, cell_style),
            ]));
        }
    }
    lines
}

/// The detail block for a selected subagent (tui-rework 14, item 3): full agent
/// id, parent session, started/up, its worktree root when a `worktree:<session>:
/// <agent>` root source matches, and its batch-derived status.
fn subagent_detail(
    session_id: &str,
    agent_id: &str,
    snapshot: &Snapshot,
    theme: &Theme,
    now: DateTime<Utc>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![title("Subagent".to_string(), theme)];
    lines.extend(full_id_lines("agent", agent_id, theme, width));
    lines.extend(full_id_lines("parent session", session_id, theme, width));
    let Some(sub) = snapshot
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .and_then(|s| s.subagents.iter().find(|a| a.id == agent_id))
    else {
        // The subagent left between snapshot and selection — name what we have.
        lines.push(kv("status", "gone".to_string(), theme));
        return lines;
    };
    let (cell, _) = status_cell(sub.status, theme);
    lines.push(kv("status", cell, theme));
    lines.push(kv(
        "up",
        format!("{} ago", elapsed_at(&sub.started_at, now)),
        theme,
    ));
    // The subagent's own worktree root, when a `worktree:<session>:<agent>`
    // contributor holds one — the root source class the daemon stamps for a
    // subagent's mounted worktree (tui-rework 14, item 3).
    let marker = format!("worktree:{session_id}:{agent_id}");
    if let Some(root) = snapshot
        .roots
        .iter()
        .find(|r| r.sources.iter().any(|src| src == &marker))
    {
        lines.push(kv("worktree", root.path.clone(), theme));
    }
    lines
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn contributor_label_summarizes_classes() {
        let sources = vec![
            "hook".to_string(),
            "mcp:aaa".to_string(),
            "mcp:bbb".to_string(),
            "worktree:s:a".to_string(),
            "ephemeral:query".to_string(),
        ];
        let label = contributor_label(&sources);
        assert!(label.contains("hook"));
        assert!(label.contains("mcp:2"));
        assert!(label.contains("worktree:1"));
        assert!(label.contains("ephemeral"));
    }

    /// Item 6b: a single MCP contributor is labelled by its session tag — the
    /// two roots one session mounts share the tag and stay distinguishable from
    /// another session's — while a long opaque id elides to its tail.
    #[test]
    fn contributor_label_tags_single_mcp_session() {
        let catenary_pair = vec!["mcp:27".to_string()];
        let lattice_pair = vec!["mcp:151".to_string()];
        assert_eq!(contributor_label(&catenary_pair), "[mcp:27]");
        assert_eq!(contributor_label(&lattice_pair), "[mcp:151]");
        assert_ne!(
            contributor_label(&catenary_pair),
            contributor_label(&lattice_pair),
            "distinct sessions carry distinct labels",
        );
        let long = vec!["mcp:0123456789a76a".to_string()];
        assert_eq!(contributor_label(&long), "[mcp:…a76a]");
    }

    /// Item 6c: the lifetime badge names the root's lifetime class; an MCP-only
    /// root carries none (its `[mcp:…]` tag already conveys the lifetime).
    #[test]
    fn lifetime_badge_reflects_root_lifetime() {
        let hook = vec!["hook".to_string(), "mcp:3".to_string()];
        assert_eq!(lifetime_badge(&hook, false), "[pin]");
        let wt = vec!["worktree:s:a".to_string()];
        assert_eq!(lifetime_badge(&wt, false), "[worktree]");
        let eph = vec!["ephemeral:query".to_string()];
        assert_eq!(lifetime_badge(&eph, true), "[activity]");
        let mcp_only = vec!["mcp:3".to_string()];
        assert_eq!(lifetime_badge(&mcp_only, false), "");
    }

    fn session_with(status: SessionStatus, last_seen: &str, summary: &str) -> SessionEntry {
        SessionEntry {
            id: "sess-1".to_string(),
            last_seen: last_seen.to_string(),
            status,
            last_action: Some(crate::state_snapshot::LastAction {
                summary: summary.to_string(),
                at: last_seen.to_string(),
            }),
            ..SessionEntry::default()
        }
    }

    /// Item 2: the `diagnostics: …` summary renders only while the batch that
    /// produced it is current — never on an idle or idle-stale session — while a
    /// non-batch action (`edited …`) always renders.
    #[test]
    fn diagnostics_summary_scoped_to_its_batch() {
        let now = crate::tui::format::parse_iso("2026-07-08T12:00:30Z").expect("iso");
        let fresh = "2026-07-08T12:00:00Z";
        let stale = "2026-07-08T11:00:00Z";
        let diag = "diagnostics: 0 errors, 0 warnings";

        let working = session_with(SessionStatus::Working, fresh, diag);
        assert_eq!(
            current_action_summary(&working, now),
            Some(diag),
            "current batch → summary shows",
        );
        let idle = session_with(SessionStatus::Idle, fresh, diag);
        assert_eq!(
            current_action_summary(&idle, now),
            None,
            "batch closed → the zero-count line must not loiter",
        );
        let gone_stale = session_with(SessionStatus::Working, stale, diag);
        assert_eq!(
            current_action_summary(&gone_stale, now),
            None,
            "idle-stale session → summary dropped",
        );
        let edited = session_with(SessionStatus::Idle, fresh, "edited src/db.rs");
        assert_eq!(
            current_action_summary(&edited, now),
            Some("edited src/db.rs"),
            "a non-diagnostics action is not batch-scoped",
        );
    }

    /// Item 1 render leg: the status cell names `working` distinctly from
    /// `editing`, and an unknown future status renders quiet — never a false
    /// `editing`.
    #[test]
    fn status_cell_names_working_and_degrades_unknown() {
        let theme = Theme::new();
        assert_eq!(status_cell(SessionStatus::Editing, &theme).0, "editing");
        assert_eq!(status_cell(SessionStatus::Working, &theme).0, "working");
        assert_eq!(status_cell(SessionStatus::Idle, &theme).0, "idle");
        assert_eq!(status_cell(SessionStatus::Unknown, &theme).0, "idle");
    }

    /// Item 3: the subagent tree row carries the capability-aware status cell,
    /// and selecting a subagent renders a detail block — full agent id, parent
    /// session, status, and its worktree root when the `worktree:<session>:
    /// <agent>` source matches.
    #[test]
    fn subagent_row_and_detail_render_status_and_worktree() {
        let theme = Theme::new();
        let now = crate::tui::format::parse_iso("2026-07-08T12:01:00Z").expect("iso");
        let sub = crate::state_snapshot::Subagent {
            id: "agent-7da239b1-d3c7-42b7-a7a4-38b4205f576a".to_string(),
            started_at: "2026-07-08T12:00:00Z".to_string(),
            status: SessionStatus::Editing,
            ..crate::state_snapshot::Subagent::default()
        };
        let line = subagent_line(&sub, 80, &theme, now);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("editing"),
            "row carries the status cell: {text}"
        );

        let mut snapshot = Snapshot::default();
        snapshot.sessions.push(SessionEntry {
            id: "sess-1".to_string(),
            subagents: vec![sub.clone()],
            ..SessionEntry::default()
        });
        snapshot.roots.push(crate::state_snapshot::RootEntry {
            path: "/wt/agents/x".to_string(),
            sources: vec![format!("worktree:sess-1:{}", sub.id)],
            ..crate::state_snapshot::RootEntry::default()
        });
        let lines = subagent_detail("sess-1", &sub.id, &snapshot, &theme, now, 200);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains(&sub.id), "full agent id present");
        assert!(text.contains("sess-1"), "parent session named");
        assert!(text.contains("editing"), "batch-derived status present");
        assert!(text.contains("/wt/agents/x"), "worktree root resolved");
    }

    /// Item 4: the session detail renders the FULL id; a long id wraps to the
    /// pane width via 12's wrap machinery, with no emitted line overflowing.
    #[test]
    fn session_detail_renders_full_session_id_wrapped() {
        let theme = Theme::new();
        let now = crate::tui::format::parse_iso("2026-07-08T12:01:00Z").expect("iso");
        let id = "7da239b1-d3c7-42b7-a7a4-38b4205f576a-aa68869b04e4520be";
        let mut snapshot = Snapshot::default();
        snapshot.sessions.push(SessionEntry {
            id: id.to_string(),
            last_seen: "2026-07-08T12:00:50Z".to_string(),
            ..SessionEntry::default()
        });
        let width = 24;
        let lines = session_detail(id, &snapshot, &theme, now, width);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        let compact: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains(
                &id.chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
            ),
            "the FULL id is present across wrapped lines",
        );
        for line in &lines {
            assert!(
                crate::tui::format::spans_width(&line.spans) <= width,
                "no emitted line exceeds the pane width",
            );
        }
    }

    /// The binary line names the resolved path — "where is it?" — not bare
    /// presence (ticket 11 item 5); an unresolvable command keeps the honest
    /// NOT-found wording.
    #[test]
    fn server_detail_names_the_resolved_binary_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("mock-server-bin");
        std::fs::write(&bin, "").expect("touch binary");

        let mut config = Config::default();
        config.server.insert(
            "mock-server".to_string(),
            crate::config::ServerDef {
                command: bin.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        config.server.insert(
            "gone-server".to_string(),
            crate::config::ServerDef {
                command: "definitely-not-a-real-binary-xyz".to_string(),
                ..Default::default()
            },
        );

        let snapshot = Snapshot::default();
        let render = |name: &str| -> String {
            server_detail(
                name,
                &snapshot,
                Some(&config),
                &[],
                &Theme::new(),
                chrono::Utc::now(),
            )
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref().to_string())
            .collect::<String>()
        };

        let found = render("mock-server");
        assert!(
            found.contains("mock-server-bin"),
            "binary line names the resolved path: {found}"
        );
        assert!(
            !found.contains("found on $PATH"),
            "presence-only wording replaced when resolved: {found}"
        );

        let missing = render("gone-server");
        assert!(
            missing.contains("NOT found on $PATH"),
            "unresolvable command keeps the honest wording: {missing}"
        );
    }

    #[test]
    fn selected_problem_wraps_full_text_unselected_truncates() {
        use crate::health::FindingCode;
        use crate::tui::format::spans_width;

        let long = "a diagnostic message that is far too long to fit on a single \
            line of the problems pane and therefore must wrap when selected";
        let rows = vec![ProblemRow {
            code: FindingCode::ServerRoutedBroken,
            severity: Severity::Error,
            message: long.to_string(),
            fix_it: None,
            owner: Owner::Global,
            is_suggestion: false,
        }];
        let theme = Theme::new();
        let width = 40usize;

        // Selected: the entry wraps to multiple lines, each within the width, and
        // the full message survives across them.
        let sel = crate::tui::render::problem_entries(&rows, width, &theme, Some(0));
        let entry = &sel[0];
        assert!(
            entry.len() > 1,
            "a long selected row wraps to multiple lines, got {}",
            entry.len()
        );
        for line in entry {
            assert!(
                spans_width(&line.spans) <= width,
                "every wrapped line is within the pane width"
            );
        }
        let joined: String = entry
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            joined.contains("must wrap when selected"),
            "the full message is present when selected: {joined}"
        );
        assert!(
            !joined.contains('…'),
            "no truncation ellipsis when selected"
        );

        // Unselected: the same row is one line, truncated with `…`.
        let plain = crate::tui::render::problem_entries(&rows, width, &theme, None);
        assert_eq!(plain[0].len(), 1, "unselected row stays one line");
        let head: String = plain[0][0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            head.ends_with('…'),
            "unselected row truncates with `…`: {head}"
        );
        assert!(spans_width(&plain[0][0].spans) <= width, "within width");
    }

    #[test]
    fn severity_glyph_and_style_distinguish_fatal_from_error() {
        assert_eq!(severity_glyph(Severity::Fatal), "✗");
        assert!(
            severity_style(&Theme::new(), Severity::Fatal)
                .add_modifier
                .contains(Modifier::BOLD),
            "fatal is bold so it reads beyond color",
        );
        assert!(
            !severity_style(&Theme::new(), Severity::Error)
                .add_modifier
                .contains(Modifier::BOLD),
        );
    }

    #[test]
    fn verdict_working_is_green_and_honest_about_suggestions() {
        let v = Verdict {
            suggestion: 2,
            ..Verdict::default()
        };
        let spans = verdict_spans(v, true, &Theme::new());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("working"), "green verdict: {text}");
        assert!(
            text.contains("2 suggestions"),
            "honest suggestion tail: {text}"
        );
    }

    #[test]
    fn install_overlay_previews_pinned_command_and_verification() {
        use crate::install::{BlessedRecipe, InstallPlan};
        use crate::recipes::{
            BlessedEntry, BlessedManifest, Ecosystem, InstallRecipe, VerificationTier,
        };

        let recipe = InstallRecipe {
            ecosystem: Ecosystem::Cargo,
            package: "taplo-cli".to_string(),
            version: "0.10.0".to_string(),
            tier: VerificationTier::CargoLocked,
            draft: true,
            hash: None,
            note: None,
            runtime: None,
        };
        let mut manifest = BlessedManifest::default();
        manifest.blessed.insert(
            "taplo".to_string(),
            BlessedEntry {
                version: "0.10.0".to_string(),
                platform: "linux-x86_64".to_string(),
                date: "2026-07-07".to_string(),
                tier: None,
            },
        );
        let blessed = BlessedRecipe::resolve("taplo", &recipe, &manifest).expect("blessed");
        let plan = InstallPlan::resolve(&blessed).expect("plan");
        let state = InstallState::new("taplo".to_string(), Ok(plan));

        let lines = install_overlay_lines(&state, &Theme::new());
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("Install taplo?"), "titles the server: {text}");
        assert!(
            text.contains("cargo install taplo-cli --version =0.10.0 --locked"),
            "shows the exact pinned command: {text}",
        );
        assert!(text.contains("--locked"), "states the verification: {text}");
        assert!(
            text.contains("Enter") && text.contains("install"),
            "offers explicit consent: {text}",
        );
    }
}
