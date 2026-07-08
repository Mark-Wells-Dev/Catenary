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

/// Compact contributor label from a root's sources (`[hook mcp:3 worktree:1]`).
#[must_use]
pub fn contributor_label(sources: &[String]) -> String {
    let (mut hook, mut mcp, mut worktree, mut ephemeral, mut other) = (false, 0, 0, false, 0);
    for s in sources {
        if s == "hook" {
            hook = true;
        } else if s.starts_with("mcp:") {
            mcp += 1;
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
    if mcp > 0 {
        parts.push(format!("mcp:{mcp}"));
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
    match entry.status {
        SessionStatus::Editing => ("editing".to_string(), theme.session_active),
        SessionStatus::Diagnostics => ("diagnostics".to_string(), theme.accent),
        SessionStatus::Idle => ("idle".to_string(), theme.session_meta),
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
        Row::Server(e) => server_line(e, width, theme, icons, now),
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
        Row::Subagent(s) => Line::from(vec![Span::styled(
            format!(
                "{}⤷ {}  up {}",
                indent(2),
                short_id(&s.id),
                elapsed_at(&s.started_at, now)
            ),
            theme.session_meta,
        )]),
    }
}

fn root_line(r: &RootRow, width: usize, theme: &Theme, icons: &IconSet) -> Line<'static> {
    let glyph = if r.expanded {
        &icons.workspace_open
    } else {
        &icons.workspace_closed
    };
    let mut left = vec![
        Span::raw("  ".to_string()),
        Span::styled(format!("{glyph} "), theme.accent),
        Span::styled(basename(&r.path).to_string(), theme.text),
    ];
    let label = contributor_label(&r.sources);
    if !label.is_empty() {
        left.push(Span::styled(format!("  {label}"), theme.muted));
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
    super::format::justify(left, right, width)
}

fn server_line(
    e: &ServerEntry,
    width: usize,
    theme: &Theme,
    icons: &IconSet,
    now: DateTime<Utc>,
) -> Line<'static> {
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

fn client_line(c: &ClientRow, width: usize, theme: &Theme, icons: &IconSet) -> Line<'static> {
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
    super::format::justify(left, right, width)
}

fn session_line(
    s: &SessionEntry,
    width: usize,
    theme: &Theme,
    now: DateTime<Utc>,
) -> Line<'static> {
    let (cell, cell_style) = session_status_cell(s, theme, now);
    let mut left = vec![
        Span::raw(indent(1)),
        Span::styled(short_id(&s.id), theme.text),
    ];
    if let Some(a) = &s.last_action {
        left.push(Span::styled(format!("  {}", a.summary), theme.muted));
    }
    let right = vec![Span::styled(cell, cell_style)];
    super::format::justify(left, right, width)
}

// ── Problems pane ────────────────────────────────────────────────────

/// Render the problems pane as entry groups (one or two lines each): the
/// labelled finding, then its fix-it indented. A suggestion tail renders with a
/// dim header so it can never be mistaken for a problem.
#[must_use]
pub fn problem_entries(
    rows: &[ProblemRow],
    width: usize,
    theme: &Theme,
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
        let head_avail = width.saturating_sub(label.chars().count());
        let mut lines = vec![Line::from(vec![
            Span::styled(label, style),
            Span::styled(truncate_to_width(&r.message, head_avail), style),
        ])];
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
        Some(EntityKey::Session(id)) => session_detail(id, snapshot, theme, now),
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
    let mut servers: Vec<&ServerEntry> = snapshot
        .servers
        .iter()
        .filter(|s| s.scope_root == path)
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
        lines.push(Line::from(vec![
            Span::styled(format!("    {} → {}  ", s.language, s.server), theme.text),
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

fn session_detail(
    id: &str,
    snapshot: &Snapshot,
    theme: &Theme,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let Some(s) = snapshot.sessions.iter().find(|s| s.id == id) else {
        return vec![title(format!("Session  {}", short_id(id)), theme)];
    };
    let mut lines = vec![title(format!("Session  {}", short_id(id)), theme)];
    lines.push(kv("client", s.client.name.clone(), theme));
    let (cell, _) = session_status_cell(s, theme, now);
    lines.push(kv("status", cell, theme));
    lines.push(kv(
        "last seen",
        format!("{} ago", elapsed_at(&s.last_seen, now)),
        theme,
    ));
    if let Some(a) = &s.last_action {
        lines.push(kv(
            "last action",
            format!("{} ({} ago)", a.summary, elapsed_at(&a.at, now)),
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
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "    ⤷ {}  up {}",
                    short_id(&sub.id),
                    elapsed_at(&sub.started_at, now)
                ),
                theme.session_meta,
            )]));
        }
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
