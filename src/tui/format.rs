// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Message formatting helpers for the TUI.
//!
//! Styled and plain-text formatters for single messages, merged pairs,
//! and collapsed runs of protocol messages.

use ratatui::text::{Line, Span};

use super::category;
use super::icons::{IconSet, basename, diag_style, tool_icon};
use super::pipeline::{DisplayEntry, SegmentPosition};
use super::theme::Theme;
use crate::session::SessionMessage;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Format a `started_at` timestamp as a human-readable duration.
#[must_use]
pub fn format_ago(started: chrono::DateTime<chrono::Utc>) -> String {
    let elapsed = chrono::Utc::now()
        .signed_duration_since(started)
        .num_seconds()
        .max(0);
    if elapsed < 60 {
        format!("{elapsed}s ago")
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

// ── Single message formatters ────────────────────────────────────────────

/// Build a styled [`Line`] for a protocol message.
///
/// Icons are intentionally omitted — single messages are raw protocol
/// records. Icons appear only in collapsed run headers where they
/// replace the method name as an at-a-glance category signal.
#[must_use]
pub fn format_message_styled(
    msg: &SessionMessage,
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let ts = msg.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    match msg.r#type.as_str() {
        "lsp" => {
            let mut spans = vec![
                ts_span,
                Span::styled(format!("[{}] ", msg.server), theme.accent),
                Span::styled(msg.method.clone(), theme.text),
            ];
            if msg.method == "$/progress"
                && let Some(detail) = progress_suffix(&msg.payload)
            {
                spans.push(Span::styled(format!(" ({detail})"), theme.muted));
            }
            Line::from(spans)
        }
        "mcp" => {
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                let icon = tool_icon(tool_name, icons);
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), theme.success),
                    Span::styled(tool_name.to_string(), theme.text),
                ])
            } else {
                Line::from(vec![
                    ts_span,
                    Span::styled("[mcp] ".to_string(), theme.text),
                    Span::styled(msg.method.clone(), theme.text),
                ])
            }
        }
        "hook" => {
            if let Some(count_val) = msg.payload.get("count") {
                let count = count_val.as_u64().unwrap_or(0);
                let file = msg
                    .payload
                    .get("file")
                    .and_then(|f| f.as_str())
                    .unwrap_or(&msg.method);
                let base = basename(file);
                if count == 0 {
                    Line::from(vec![
                        ts_span,
                        Span::styled(icons.diag_ok.clone(), theme.success),
                        Span::styled(base.to_string(), theme.text),
                    ])
                } else {
                    let preview = msg
                        .payload
                        .get("preview")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "diagnostic count is always small"
                    )]
                    let (icon, style) = diag_style(count as usize, preview, icons, theme);
                    let label = format!("{count} diagnostic{}", if count == 1 { "" } else { "s" });
                    Line::from(vec![
                        ts_span,
                        Span::styled(icon.to_string(), style),
                        Span::styled(format!("{base}: "), theme.text),
                        Span::styled(label, style),
                    ])
                }
            } else {
                Line::from(vec![
                    ts_span,
                    Span::styled("[hook] ".to_string(), theme.text),
                    Span::styled(msg.method.clone(), theme.text),
                ])
            }
        }
        other => Line::from(vec![
            ts_span,
            Span::styled(format!("[{other}] "), theme.text),
            Span::styled(msg.method.clone(), theme.text),
        ]),
    }
}

/// Plain-text message summary (used for filter matching).
#[must_use]
pub fn format_message_plain(msg: &SessionMessage) -> String {
    let ts = msg.timestamp.format("%H:%M:%S");

    match msg.r#type.as_str() {
        "lsp" => {
            let detail = if msg.method == "$/progress" {
                progress_suffix(&msg.payload)
            } else {
                None
            };
            detail.map_or_else(
                || format!("{ts} [{}] {}", msg.server, msg.method),
                |d| format!("{ts} [{}] {} ({d})", msg.server, msg.method),
            )
        }
        "mcp" => {
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                format!("{ts} {tool_name}")
            } else {
                format!("{ts} [mcp] {}", msg.method)
            }
        }
        "hook" => msg.payload.get("count").map_or_else(
            || format!("{ts} [hook] {}", msg.method),
            |count_val| {
                let count = count_val.as_u64().unwrap_or(0);
                let file = msg
                    .payload
                    .get("file")
                    .and_then(|f| f.as_str())
                    .unwrap_or(&msg.method);
                let base = basename(file);
                if count == 0 {
                    format!("{ts} {base}")
                } else {
                    format!("{ts} {base}: {count} diagnostics")
                }
            },
        ),
        other => format!("{ts} [{other}] {}", msg.method),
    }
}

// ── Duration + result helpers ────────────────────────────────────────────

/// Format a timing delta as a compact string.
///
/// Sub-10s: one decimal place (`0.5s`, `3.2s`).
/// 10s+: integer seconds (`12s`, `45s`).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "millisecond timing values never exceed f64 mantissa range"
)]
pub fn format_duration_short(millis: i64) -> String {
    let millis = millis.max(0);
    if millis < 10_000 {
        let secs = millis as f64 / 1000.0;
        format!("{secs:.1}s")
    } else {
        format!("{}s", millis / 1000)
    }
}

/// Outcome of a merged request/response pair.
enum PairOutcome {
    Success,
    Error { message: Option<String> },
    Cancelled,
}

/// Determine the outcome of a merged pair from the response payload.
fn pair_outcome(response: &SessionMessage) -> PairOutcome {
    if response.method == "notifications/cancelled" {
        return PairOutcome::Cancelled;
    }
    if let Some(msg) = extract_jsonrpc_error(&response.payload) {
        return PairOutcome::Error { message: Some(msg) };
    }
    if response.method == "tools/call" {
        if let Some(msg) = extract_tool_error(&response.payload) {
            return PairOutcome::Error { message: Some(msg) };
        }
        // Top-level isError without content text.
        if response
            .payload
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return PairOutcome::Error { message: None };
        }
    }
    PairOutcome::Success
}

/// Extract an error message from a JSON-RPC error response.
fn extract_jsonrpc_error(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("error")?
        .get("message")?
        .as_str()
        .map(String::from)
}

/// Extract an error message from an MCP tool error response.
///
/// Looks for `result.content[0].isError == true` and returns the text.
fn extract_tool_error(payload: &serde_json::Value) -> Option<String> {
    let content = payload.get("result")?.get("content")?.as_array()?;
    let first = content.first()?;
    if first
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        first.get("text")?.as_str().map(String::from)
    } else {
        None
    }
}

// ── Tool metric extractors ───────────────────────────────────────────────

/// Extract the total line count from an MCP tool response payload.
///
/// Walks `result.content[]` and sums `.lines().count()` for every
/// `type: "text"` item. Returns `None` if the path doesn't exist
/// (non-tool response), `Some(0)` for empty text content.
fn extract_line_count(response: &SessionMessage) -> Option<usize> {
    let result = response.payload.get("result")?;
    let content = result.get("content")?.as_array()?;
    let mut total = 0;
    for item in content {
        let is_text = item
            .get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t == "text");
        if !is_text {
            continue;
        }
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            total += text.lines().count();
        }
    }
    Some(total)
}

/// Render a JSON value as a compact inline string.
///
/// Strings are quoted, numbers/bools/null are literal, and nested
/// arrays/objects are opaque (`[...]` / `{...}`).
fn compact_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(_) => "[...]".to_string(),
        serde_json::Value::Object(_) => "{...}".to_string(),
    }
}

/// Extract tool call arguments from an MCP request payload.
///
/// Returns a compact `{key: value, key2: value2}` string where keys are
/// unquoted and values use [`compact_value`] rendering.
fn extract_tool_arguments(request: &SessionMessage) -> Option<String> {
    let args = request.payload.get("params")?.get("arguments")?;
    let obj = args.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let pairs: Vec<String> = obj
        .iter()
        .map(|(k, v)| format!("{k}: {}", compact_value(v)))
        .collect();
    Some(format!("{{{}}}", pairs.join(", ")))
}

/// Build a metrics parenthetical string for a tool call pair.
///
/// Combines optional line count with timing into the parenthetical content.
fn format_tool_metrics(line_count: Option<usize>, timing: &str) -> String {
    line_count.map_or_else(
        || timing.to_string(),
        |n| format!("{n} line{}, {timing}", if n == 1 { "" } else { "s" }),
    )
}

// ── Pair formatters ──────────────────────────────────────────────────────

/// Build a styled [`Line`] for a merged request/response pair.
///
/// Icon-based rendering: outcome icons replace directional arrows.
/// Error messages are extracted from response payloads and shown inline.
#[must_use]
pub fn format_pair_styled(
    request: &SessionMessage,
    response: &SessionMessage,
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let ts = request.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    let delta_ms = response
        .timestamp
        .signed_duration_since(request.timestamp)
        .num_milliseconds();
    let timing = format_duration_short(delta_ms);
    let outcome = pair_outcome(response);

    // Resolve the tool name for MCP tools/call requests.
    let tool_name = if request.method == "tools/call" {
        request
            .payload
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    } else {
        None
    };

    match &outcome {
        PairOutcome::Cancelled => {
            let label = tool_name.unwrap_or(&request.method);
            let mut spans = vec![ts_span];
            match request.r#type.as_str() {
                "lsp" => {
                    spans.push(Span::styled(format!("[{}] ", request.server), theme.accent));
                }
                "mcp" if tool_name.is_none() => {
                    spans.push(Span::styled("[mcp] ".to_string(), theme.text));
                }
                _ => {}
            }
            spans.push(Span::styled(icons.cancelled.clone(), theme.muted));
            spans.push(Span::styled(label.to_string(), theme.text));
            spans.push(Span::styled(format!(" (cancelled, {timing})"), theme.muted));
            Line::from(spans)
        }
        PairOutcome::Error { message } => {
            let label = tool_name.unwrap_or(&request.method);
            let error_suffix = message
                .as_deref()
                .map_or(String::new(), |m| format!(": {m}"));
            let mut spans = vec![ts_span];
            match request.r#type.as_str() {
                "lsp" => {
                    spans.push(Span::styled(format!("[{}] ", request.server), theme.accent));
                }
                "mcp" if tool_name.is_none() => {
                    spans.push(Span::styled("[mcp] ".to_string(), theme.text));
                }
                _ => {}
            }
            spans.push(Span::styled(icons.proto_error.clone(), theme.error));
            spans.push(Span::styled(format!("{label}{error_suffix}"), theme.text));
            spans.push(Span::styled(format!(" ({timing})"), theme.muted));
            Line::from(spans)
        }
        PairOutcome::Success => match request.r#type.as_str() {
            "lsp" => Line::from(vec![
                ts_span,
                Span::styled(format!("[{}] ", request.server), theme.accent),
                Span::styled(icons.proto_ok.clone(), theme.success),
                Span::styled(request.method.clone(), theme.text),
                Span::styled(format!(" ({timing})"), theme.muted),
            ]),
            "mcp" => {
                if let Some(name) = tool_name {
                    let icon = tool_icon(name, icons);
                    let line_count = extract_line_count(response);
                    let metrics = format_tool_metrics(line_count, &timing);
                    let args = extract_tool_arguments(request);
                    let mut spans = vec![
                        ts_span,
                        Span::styled(icon.to_string(), theme.success),
                        Span::styled(name.to_string(), theme.text),
                        Span::styled(format!(" ({metrics})"), theme.muted),
                    ];
                    if let Some(args_str) = args {
                        spans.push(Span::styled(format!(" {args_str}"), theme.muted));
                    }
                    Line::from(spans)
                } else {
                    Line::from(vec![
                        ts_span,
                        Span::styled(icons.proto_ok.clone(), theme.success),
                        Span::styled(request.method.clone(), theme.text),
                        Span::styled(format!(" ({timing})"), theme.muted),
                    ])
                }
            }
            other => Line::from(vec![
                ts_span,
                Span::styled(format!("[{other}] "), theme.text),
                Span::styled(icons.proto_ok.clone(), theme.success),
                Span::styled(request.method.clone(), theme.text),
                Span::styled(format!(" ({timing})"), theme.muted),
            ]),
        },
    }
}

/// Plain-text summary for a merged request/response pair (filter matching, yank).
#[must_use]
pub fn format_pair_plain(request: &SessionMessage, response: &SessionMessage) -> String {
    let ts = request.timestamp.format("%H:%M:%S");
    let delta_ms = response
        .timestamp
        .signed_duration_since(request.timestamp)
        .num_milliseconds();
    let timing = format_duration_short(delta_ms);
    let outcome = pair_outcome(response);

    let tool_name = if request.method == "tools/call" {
        request
            .payload
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    } else {
        None
    };

    let args_suffix = tool_name
        .and_then(|_| extract_tool_arguments(request))
        .map_or(String::new(), |a| format!(" {a}"));

    match &outcome {
        PairOutcome::Cancelled => {
            let label = tool_name.unwrap_or(&request.method);
            match request.r#type.as_str() {
                "lsp" => format!(
                    "{ts} [{}] {label} (cancelled, {timing}){args_suffix}",
                    request.server
                ),
                _ => format!("{ts} {label} (cancelled, {timing}){args_suffix}"),
            }
        }
        PairOutcome::Error { message } => {
            let label = tool_name.unwrap_or(&request.method);
            let error_suffix = message
                .as_deref()
                .map_or(String::new(), |m| format!(": {m}"));
            match request.r#type.as_str() {
                "lsp" => format!(
                    "{ts} [{}] {label}{error_suffix} ({timing}){args_suffix}",
                    request.server
                ),
                _ => format!("{ts} {label}{error_suffix} ({timing}){args_suffix}"),
            }
        }
        PairOutcome::Success => match request.r#type.as_str() {
            "lsp" => format!("{ts} [{}] {} ({timing})", request.server, request.method),
            "mcp" => tool_name.map_or_else(
                || format!("{ts} {} ({timing})", request.method),
                |name| {
                    let line_count = extract_line_count(response);
                    let metrics = format_tool_metrics(line_count, &timing);
                    format!("{ts} {name} ({metrics}){args_suffix}")
                },
            ),
            other => format!("{ts} [{other}] {} ({timing})", request.method),
        },
    }
}

// ── Progress detail ──────────────────────────────────────────────────────

/// Extract payload detail from a single `$/progress` message as a
/// parenthesized suffix to append after the method name.
///
/// Includes title, message, and percentage when present. Returns `None`
/// when the payload has no extractable detail.
fn progress_suffix(payload: &serde_json::Value) -> Option<String> {
    let value = payload.get("value")?;
    let kind = value.get("kind").and_then(|k| k.as_str());
    let title = value.get("title").and_then(|t| t.as_str());
    let message = value.get("message").and_then(|m| m.as_str());
    let pct = value.get("percentage").and_then(serde_json::Value::as_u64);

    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = title {
        parts.push(t.to_string());
    }
    if let Some(m) = message {
        parts.push(m.to_string());
    }
    if let Some(p) = pct {
        parts.push(format!("{p}%"));
    }
    if kind == Some("end") && parts.is_empty() {
        parts.push("done".to_string());
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Format the detail portion of a collapsed progress run.
///
/// Combines message count with an optional percentage range.
fn format_progress_detail(count: usize, first_pct: Option<u64>, last_pct: Option<u64>) -> String {
    let count_label = format!("{count} message{}", if count == 1 { "" } else { "s" });
    match (first_pct, last_pct) {
        (Some(f), Some(l)) if f != l => format!("{count_label}, {f}%\u{2192}{l}%"),
        (Some(p), _) | (_, Some(p)) => format!("{count_label}, {p}%"),
        _ => count_label,
    }
}

// ── Collapsed run formatters ─────────────────────────────────────────────

/// Build a styled [`Line`] for a collapsed run of messages.
///
/// Category-specific rendering for progress, sync, lifecycle, log, and
/// MCP init runs. Falls back to generic `method (N messages)` for
/// protocol-level runs. Icons appear here (not in single-message
/// formatters) — collapsed headers are summaries where an icon earns
/// its space as an at-a-glance category signal.
///
/// Uses the last message's timestamp (most recent in the run).
#[must_use]
pub fn format_collapsed_styled(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
    count: usize,
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let last = &messages[end];
    let ts = last.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    let key = category::collapse_key(&messages[start]);
    let key_str = key.as_deref().unwrap_or("");

    if key_str.starts_with("progress:") {
        let title = category::extract_progress_title(messages, start, end);
        let (first_pct, last_pct) = category::extract_progress_pct_range(messages, start, end);
        let detail = format_progress_detail(count, first_pct, last_pct);
        let server = &messages[start].server;
        Line::from(vec![
            ts_span,
            Span::styled(format!("[{server}] "), theme.accent),
            Span::styled(icons.progress.clone(), theme.muted),
            Span::styled(title, theme.text),
            Span::styled(format!(" ({detail})"), theme.muted),
        ])
    } else if key_str.starts_with("log:") {
        let label = category::log_level_label(key_str);
        let count_label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        let server = &messages[start].server;
        let mut spans = vec![ts_span, Span::styled(format!("[{server}] "), theme.accent)];
        if label == "info" {
            spans.push(Span::styled(icons.log_info.clone(), theme.info));
        }
        spans.push(Span::styled(label.to_string(), theme.text));
        spans.push(Span::styled(format!(" ({count_label})"), theme.muted));
        Line::from(spans)
    } else if key_str.starts_with("sync:") {
        let file = category::extract_sync_basename(messages, start, end).unwrap_or_default();
        let ops = category::extract_sync_operations(messages, start, end);
        let ops_str = ops.join(", ");
        let server = &messages[start].server;
        Line::from(vec![
            ts_span,
            Span::styled(format!("[{server}] "), theme.accent),
            Span::styled(format!("sync {file}"), theme.text),
            Span::styled(format!(" ({ops_str})"), theme.muted),
        ])
    } else if key_str.starts_with("lifecycle:") {
        let server = &messages[start].server;
        Line::from(vec![
            ts_span,
            Span::styled(format!("[{server}] "), theme.accent),
            Span::styled(icons.session_started.clone(), theme.accent),
            Span::styled("initialized".to_string(), theme.text),
        ])
    } else if key_str == "init:mcp" {
        Line::from(vec![
            ts_span,
            Span::styled(icons.session_started.clone(), theme.accent),
            Span::styled("mcp initialized".to_string(), theme.text),
        ])
    } else {
        // Generic fallback (proto: or unknown).
        let label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        match last.r#type.as_str() {
            "lsp" => Line::from(vec![
                ts_span,
                Span::styled(format!("[{}] ", last.server), theme.accent),
                Span::styled(last.method.clone(), theme.text),
                Span::styled(format!(" ({label})"), theme.muted),
            ]),
            "mcp" => Line::from(vec![
                ts_span,
                Span::styled("[mcp] ".to_string(), theme.text),
                Span::styled(last.method.clone(), theme.text),
                Span::styled(format!(" ({label})"), theme.muted),
            ]),
            other => Line::from(vec![
                ts_span,
                Span::styled(format!("[{other}] "), theme.text),
                Span::styled(last.method.clone(), theme.text),
                Span::styled(format!(" ({label})"), theme.muted),
            ]),
        }
    }
}

/// Plain-text summary for a collapsed run (filter matching, yank).
///
/// Category-specific rendering matching [`format_collapsed_styled`].
#[must_use]
pub fn format_collapsed_plain(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
    count: usize,
) -> String {
    let last = &messages[end];
    let ts = last.timestamp.format("%H:%M:%S");

    let key = category::collapse_key(&messages[start]);
    let key_str = key.as_deref().unwrap_or("");

    if key_str.starts_with("progress:") {
        let title = category::extract_progress_title(messages, start, end);
        let (first_pct, last_pct) = category::extract_progress_pct_range(messages, start, end);
        let detail = format_progress_detail(count, first_pct, last_pct);
        format!("{ts} [{}] {title} ({detail})", messages[start].server)
    } else if key_str.starts_with("log:") {
        let label = category::log_level_label(key_str);
        let count_label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        format!("{ts} [{}] {label} ({count_label})", messages[start].server)
    } else if key_str.starts_with("sync:") {
        let file = category::extract_sync_basename(messages, start, end).unwrap_or_default();
        let ops = category::extract_sync_operations(messages, start, end);
        let ops_str = ops.join(", ");
        format!("{ts} [{}] sync {file} ({ops_str})", messages[start].server)
    } else if key_str.starts_with("lifecycle:") {
        format!("{ts} [{}] initialized", messages[start].server)
    } else if key_str == "init:mcp" {
        format!("{ts} mcp initialized")
    } else {
        // Generic fallback (proto: or unknown).
        let label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        match last.r#type.as_str() {
            "lsp" => format!("{ts} [{}] {} ({label})", last.server, last.method),
            "mcp" => format!("{ts} [mcp] {} ({label})", last.method),
            other => format!("{ts} [{other}] {} ({label})", last.method),
        }
    }
}

// ── Scope formatters ──────────────────────────────────────────────────

/// Build a scope header line for a Paired parent entry.
///
/// Outcome-aware: icons reflect success/error/cancellation with error
/// messages extracted from the response payload.
fn format_scope_pair(
    req: &SessionMessage,
    resp: &SessionMessage,
    position: SegmentPosition,
    children_label: &str,
    has_metrics: bool,
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let ts = req.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    let delta_ms = resp
        .timestamp
        .signed_duration_since(req.timestamp)
        .num_milliseconds();
    let timing = format_duration_short(delta_ms);
    let outcome = pair_outcome(resp);

    let tool_name = if req.method == "tools/call" {
        req.payload
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    } else {
        None
    };

    let label = tool_name.unwrap_or(&req.method);
    let name = segment_tool_name(label, position);

    // Line count metrics only for completed tool calls (Only/Last positions).
    let line_count = if has_metrics && tool_name.is_some() {
        extract_line_count(resp)
    } else {
        None
    };

    let (icon, icon_style, name_text, meta) = match &outcome {
        PairOutcome::Cancelled => {
            let meta = if has_metrics {
                format!(" (cancelled, {children_label}, {timing})")
            } else {
                format!(" (cancelled, {children_label})")
            };
            (icons.cancelled.clone(), theme.muted, name, meta)
        }
        PairOutcome::Error { message } => {
            let error_suffix = message
                .as_deref()
                .map_or(String::new(), |m| format!(": {m}"));
            let meta = if has_metrics {
                format!(" ({children_label}, {timing})")
            } else {
                format!(" ({children_label})")
            };
            (
                icons.proto_error.clone(),
                theme.error,
                format!("{name}{error_suffix}"),
                meta,
            )
        }
        PairOutcome::Success => {
            let meta = if has_metrics {
                let metrics = format_tool_metrics(line_count, &timing);
                format!(" ({metrics}, {children_label})")
            } else {
                format!(" ({children_label})")
            };
            let icon = tool_name.map_or_else(
                || icons.proto_ok.clone(),
                |tn| tool_icon(tn, icons).to_string(),
            );
            (icon, theme.success, name, meta)
        }
    };

    let args = if tool_name.is_some() {
        extract_tool_arguments(req)
    } else {
        None
    };

    let mut spans = vec![ts_span];
    if req.r#type == "lsp" {
        spans.push(Span::styled(format!("[{}] ", req.server), theme.accent));
    }
    spans.push(Span::styled(icon, icon_style));
    spans.push(Span::styled(name_text, theme.text));
    spans.push(Span::styled(meta, theme.muted));
    if let Some(args_str) = args {
        spans.push(Span::styled(format!(" {args_str}"), theme.muted));
    }
    Line::from(spans)
}

/// Build a styled [`Line`] for a scope header (parent with grouped children).
///
/// Outcome-aware rendering: icons reflect success/error/cancellation.
/// For tool calls: `HH:MM:SS icon tool_name (N children, Xs)`.
/// Error messages are extracted from the response payload and shown inline.
///
/// Segment position controls the ellipsis convention on tool names:
/// - `Only`: `tool_name (metrics)` — full scope, no ellipsis
/// - `First`: `tool_name…` — scope opened, no metrics yet
/// - `Middle`: `…tool_name…` — continuation, no metrics yet
/// - `Last`: `…tool_name (metrics)` — final segment with metrics
#[must_use]
pub fn format_scope_styled(
    parent: &DisplayEntry,
    child_count: usize,
    position: SegmentPosition,
    messages: &[SessionMessage],
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let children_label = format!(
        "{child_count} child{}",
        if child_count == 1 { "" } else { "ren" }
    );
    let has_metrics = matches!(position, SegmentPosition::Only | SegmentPosition::Last);

    match parent {
        DisplayEntry::Paired {
            request_index,
            response_index,
            ..
        } => format_scope_pair(
            &messages[*request_index],
            &messages[*response_index],
            position,
            &children_label,
            has_metrics,
            icons,
            theme,
        ),
        DisplayEntry::Single { index, .. } => {
            let msg = &messages[*index];
            let ts = msg.timestamp.format("%H:%M:%S").to_string();
            let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                let icon = tool_icon(tool_name, icons);
                let name = segment_tool_name(tool_name, position);
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), theme.success),
                    Span::styled(name, theme.text),
                    Span::styled(format!(" ({children_label})"), theme.muted),
                ])
            } else {
                let method = segment_tool_name(&msg.method, position);
                Line::from(vec![
                    ts_span,
                    Span::styled(method, theme.text),
                    Span::styled(format!(" ({children_label})"), theme.muted),
                ])
            }
        }
        // Collapsed and Scope parents are unlikely but handle uniformly.
        _ => {
            let label = format!("scope ({children_label})");
            Line::from(vec![Span::styled(label, theme.text)])
        }
    }
}

/// Apply the segment ellipsis convention to a tool/method name.
///
/// - `Only`: `name` (no ellipsis)
/// - `First`: `name…`
/// - `Middle`: `…name…`
/// - `Last`: `…name`
fn segment_tool_name(name: &str, position: SegmentPosition) -> String {
    match position {
        SegmentPosition::Only => name.to_string(),
        SegmentPosition::First => format!("{name}\u{2026}"),
        SegmentPosition::Middle => format!("\u{2026}{name}\u{2026}"),
        SegmentPosition::Last => format!("\u{2026}{name}"),
    }
}

/// Plain-text summary for a scope header (filter matching, yank).
#[must_use]
pub fn format_scope_plain(
    parent: &DisplayEntry,
    child_count: usize,
    position: SegmentPosition,
    messages: &[SessionMessage],
) -> String {
    let children_label = format!(
        "{child_count} child{}",
        if child_count == 1 { "" } else { "ren" }
    );
    let has_metrics = matches!(position, SegmentPosition::Only | SegmentPosition::Last);

    match parent {
        DisplayEntry::Paired {
            request_index,
            response_index,
            ..
        } => {
            let req = &messages[*request_index];
            let resp = &messages[*response_index];
            let ts = req.timestamp.format("%H:%M:%S");
            let delta_ms = resp
                .timestamp
                .signed_duration_since(req.timestamp)
                .num_milliseconds();
            let timing = format_duration_short(delta_ms);
            let outcome = pair_outcome(resp);

            let tool_name = if req.method == "tools/call" {
                req.payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
            } else {
                None
            };

            let label = tool_name.unwrap_or(&req.method);
            let name = segment_tool_name(label, position);

            let prefix = if req.r#type == "lsp" {
                format!("{ts} [{}] ", req.server)
            } else {
                format!("{ts} ")
            };

            let args_suffix = tool_name
                .and_then(|_| extract_tool_arguments(req))
                .map_or(String::new(), |a| format!(" {a}"));

            let line_count = if has_metrics && tool_name.is_some() {
                extract_line_count(resp)
            } else {
                None
            };

            match &outcome {
                PairOutcome::Cancelled => {
                    if has_metrics {
                        format!(
                            "{prefix}{name} (cancelled, {children_label}, {timing}){args_suffix}"
                        )
                    } else {
                        format!("{prefix}{name} (cancelled, {children_label}){args_suffix}")
                    }
                }
                PairOutcome::Error { message } => {
                    let error_suffix = message
                        .as_deref()
                        .map_or(String::new(), |m| format!(": {m}"));
                    if has_metrics {
                        format!(
                            "{prefix}{name}{error_suffix} ({children_label}, {timing}){args_suffix}"
                        )
                    } else {
                        format!("{prefix}{name}{error_suffix} ({children_label}){args_suffix}")
                    }
                }
                PairOutcome::Success => {
                    if has_metrics {
                        let metrics = format_tool_metrics(line_count, &timing);
                        format!("{prefix}{name} ({metrics}, {children_label}){args_suffix}")
                    } else {
                        format!("{prefix}{name} ({children_label}){args_suffix}")
                    }
                }
            }
        }
        DisplayEntry::Single { index, .. } => {
            let msg = &messages[*index];
            let ts = msg.timestamp.format("%H:%M:%S");
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                let name = segment_tool_name(tool_name, position);
                format!("{ts} {name} ({children_label})")
            } else {
                let method = segment_tool_name(&msg.method, position);
                format!("{ts} {method} ({children_label})")
            }
        }
        _ => format!("scope ({children_label})"),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Utc};

    use crate::config::IconConfig;
    use crate::session::SessionMessage;
    use crate::session::test_support;

    fn make_message(r#type: &str, method: &str, server: &str) -> SessionMessage {
        test_support::message(r#type, method, server)
    }

    fn make_message_with_payload(
        r#type: &str,
        method: &str,
        server: &str,
        payload: serde_json::Value,
    ) -> SessionMessage {
        test_support::message_with_payload(r#type, method, server, payload)
    }

    #[test]
    fn test_format_ago_seconds() {
        let ts = Utc::now() - TimeDelta::seconds(30);
        assert_eq!(format_ago(ts), "30s ago");
    }

    #[test]
    fn test_format_ago_minutes() {
        let ts = Utc::now() - TimeDelta::minutes(5);
        assert_eq!(format_ago(ts), "5m ago");
    }

    #[test]
    fn test_format_ago_hours() {
        let ts = Utc::now() - TimeDelta::hours(2);
        assert_eq!(format_ago(ts), "2h ago");
    }

    #[test]
    fn test_format_ago_boundaries() {
        // Exactly 60s → minutes bucket, not "60s ago".
        let ts = Utc::now() - TimeDelta::seconds(60);
        assert_eq!(format_ago(ts), "1m ago");
        // Exactly 3600s → hours bucket, not "60m ago".
        let ts = Utc::now() - TimeDelta::seconds(3600);
        assert_eq!(format_ago(ts), "1h ago");
        // Exactly 86400s → days bucket, not "24h ago".
        let ts = Utc::now() - TimeDelta::seconds(86400);
        assert_eq!(format_ago(ts), "1d ago");
    }

    #[test]
    fn test_format_duration_short() {
        assert_eq!(format_duration_short(0), "0.0s");
        assert_eq!(format_duration_short(500), "0.5s");
        assert_eq!(format_duration_short(3200), "3.2s");
        assert_eq!(format_duration_short(9999), "10.0s");
        // Exactly 10000ms → integer format, not "10.0s".
        assert_eq!(format_duration_short(10_000), "10s");
        assert_eq!(format_duration_short(45_000), "45s");
        // Negative clamped to 0.
        assert_eq!(format_duration_short(-100), "0.0s");
    }

    // ── Message formatter tests ─────────────────────────────────────────

    #[test]
    fn test_format_message_styled_lsp() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[rust-analyzer]"),
            "should contain server name"
        );
        assert!(text.contains("textDocument/hover"), "should contain method");
        assert!(
            !text.contains("\u{2192}") && !text.contains("\u{2190}"),
            "should not contain direction arrows"
        );
    }

    #[test]
    fn test_format_message_styled_lsp_response() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            serde_json::json!({"id": 1, "result": {"contents": "fn main()"}}),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("\u{2190}") && !text.contains("\u{2192}"),
            "should not contain direction arrows"
        );
    }

    #[test]
    fn test_format_message_styled_mcp() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("grep"), "should contain tool name");
    }

    #[test]
    fn test_format_message_styled_hook() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({
                "file": "/src/lib.rs",
                "count": 2,
                "preview": "\t:12:1 [error] rustc: bad"
            }),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("lib.rs"), "should contain file basename");
        assert!(text.contains("2 diagnostics"), "should show count");
    }

    #[test]
    fn test_format_message_styled_hook_clean() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({"file": "/src/lib.rs", "count": 0}),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("lib.rs"), "should contain file basename");
        assert!(
            line.spans.iter().any(|s| s.style == theme.success),
            "clean diagnostics should use success style"
        );
    }

    #[test]
    fn test_format_message_plain() {
        let msg = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let plain = format_message_plain(&msg);
        assert!(plain.contains("[rust-analyzer]"));
        assert!(plain.contains("textDocument/hover"));
        assert!(
            !plain.contains("\u{2192}") && !plain.contains("\u{2190}"),
            "should not contain direction arrows"
        );

        let mcp_msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let plain = format_message_plain(&mcp_msg);
        assert!(plain.contains("grep"));

        let hook_msg = make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({"file": "/src/main.rs", "count": 3}),
        );
        let plain = format_message_plain(&hook_msg);
        assert!(plain.contains("main.rs"));
        assert!(plain.contains("3 diagnostics"));
    }

    // ── Progress single-message tests ──────────────────────────────────

    #[test]
    fn test_format_message_progress_begin() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({
                "token": "wid/1",
                "value": {"kind": "begin", "title": "Indexing", "percentage": 0}
            }),
        );
        let styled = format_message_styled(&msg, &icons, &theme);
        let text: String = styled.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("$/progress"), "styled should contain method");
        assert!(text.contains("Indexing"), "styled should contain title");
        assert!(text.contains("0%"), "styled should contain percentage");

        let plain = format_message_plain(&msg);
        assert!(plain.contains("$/progress"), "plain should contain method");
        assert!(plain.contains("Indexing"), "plain should contain title");
        assert!(plain.contains("0%"), "plain should contain percentage");
    }

    #[test]
    fn test_format_message_progress_report() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({
                "token": "wid/1",
                "value": {"kind": "report", "message": "42/100 crates", "percentage": 42}
            }),
        );
        let styled = format_message_styled(&msg, &icons, &theme);
        let text: String = styled.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("$/progress"), "styled should contain method");
        assert!(
            text.contains("42/100 crates"),
            "styled should contain message"
        );
        assert!(text.contains("42%"), "styled should contain percentage");

        let plain = format_message_plain(&msg);
        assert!(plain.contains("$/progress"), "plain should contain method");
        assert!(
            plain.contains("42/100 crates"),
            "plain should contain message"
        );
        assert!(plain.contains("42%"), "plain should contain percentage");
    }

    #[test]
    fn test_format_message_progress_end() {
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({
                "token": "wid/1",
                "value": {"kind": "end"}
            }),
        );
        let plain = format_message_plain(&msg);
        assert!(plain.contains("$/progress"), "should contain method");
        assert!(plain.contains("done"), "end should show done");
    }

    #[test]
    fn test_format_message_progress_begin_with_message() {
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({
                "token": "wid/1",
                "value": {"kind": "begin", "title": "Indexing", "message": "starting"}
            }),
        );
        let plain = format_message_plain(&msg);
        assert!(plain.contains("Indexing"), "should contain title");
        assert!(plain.contains("starting"), "should contain message");
    }

    #[test]
    fn test_format_message_progress_empty_value() {
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({"token": "wid/1"}),
        );
        let plain = format_message_plain(&msg);
        assert!(
            plain.contains("$/progress"),
            "empty value should fall back to method name"
        );
    }

    #[test]
    fn test_progress_suffix_bare_report() {
        // A "report" with no title, message, or percentage should return None,
        // not "done" (which is only for "end" kind).
        let payload = serde_json::json!({"value": {"kind": "report"}});
        assert_eq!(progress_suffix(&payload), None);
    }

    #[test]
    fn test_format_progress_detail_equal_percentages() {
        // When first == last, should show single percentage, not a range.
        let detail = format_progress_detail(3, Some(42), Some(42));
        assert_eq!(detail, "3 messages, 42%");
        assert!(
            !detail.contains('\u{2192}'),
            "equal percentages should not show arrow: {detail}"
        );
    }

    #[test]
    fn test_format_progress_detail_single_percentage() {
        // Only first percentage present.
        assert_eq!(format_progress_detail(2, Some(10), None), "2 messages, 10%");
        // Only last percentage present.
        assert_eq!(format_progress_detail(2, None, Some(90)), "2 messages, 90%");
        // Neither present.
        assert_eq!(format_progress_detail(2, None, None), "2 messages");
        // Singular.
        assert_eq!(format_progress_detail(1, None, None), "1 message");
    }

    // ── Collapsed rendering tests ────────────────────────────────────────

    #[test]
    fn test_format_collapsed_progress() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({"token": "ra/indexing"}),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({"token": "ra/indexing"}),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({"token": "ra/indexing"}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 2, 3, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("3 messages"),
            "should contain message count: {text}"
        );
        assert!(
            text.contains("[rust-analyzer]"),
            "should contain server name: {text}"
        );
    }

    #[test]
    fn test_format_collapsed_sync() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "textDocument/didOpen",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
            make_message_with_payload(
                "lsp",
                "textDocument/didSave",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("sync main.rs"),
            "should contain sync + basename: {text}"
        );
        assert!(
            text.contains("open, save"),
            "should contain operations: {text}"
        );
    }

    // ── Category-specific collapsed rendering tests ─────────────────────

    #[test]
    fn test_format_collapsed_progress_with_title() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "begin", "title": "Roots Scanned", "percentage": 0}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "report", "percentage": 13}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "report", "percentage": 49}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "end"}
                }),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 3, 4, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("Roots Scanned"),
            "should contain title: {text}"
        );
        assert!(
            text.contains("0%\u{2192}49%"),
            "should contain percentage range: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 3, 4);
        assert!(
            plain.contains("Roots Scanned"),
            "plain should contain title: {plain}"
        );
        assert!(
            plain.contains("0%\u{2192}49%"),
            "plain should contain percentage range: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_progress_no_percentage() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/indexing",
                    "value": {"kind": "begin", "title": "Indexing"}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/indexing",
                    "value": {"kind": "end"}
                }),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Indexing"), "should contain title: {text}");
        assert!(text.contains("2 messages"), "should contain count: {text}");
        assert!(!text.contains('%'), "should not contain percentage: {text}");

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("Indexing"),
            "plain should contain title: {plain}"
        );
        assert!(
            !plain.contains('%'),
            "plain should not contain percentage: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_sync_operations() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "textDocument/didOpen",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
            make_message_with_payload(
                "lsp",
                "textDocument/didSave",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("main.rs"), "should contain basename: {text}");
        assert!(text.contains("open"), "should contain open: {text}");
        assert!(text.contains("save"), "should contain save: {text}");

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("sync main.rs"),
            "plain should contain sync + basename: {plain}"
        );
        assert!(
            plain.contains("open, save"),
            "plain should contain operations: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_lifecycle() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message("lsp", "initialize", "shellscript"),
            make_message("lsp", "initialized", "shellscript"),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("initialized"),
            "should contain initialized: {text}"
        );
        assert!(
            text.contains("[shellscript]"),
            "should contain server name: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("initialized"),
            "plain should contain initialized: {plain}"
        );
        assert!(
            plain.contains("[shellscript]"),
            "plain should contain server name: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_mcp_init() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message("mcp", "initialize", "catenary"),
            make_message("mcp", "notifications/initialized", "catenary"),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("mcp initialized"),
            "should contain mcp initialized: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("mcp initialized"),
            "plain should contain mcp initialized: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_log_info() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "python",
                serde_json::json!({"type": 3, "message": "Loading..."}),
            ),
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "python",
                serde_json::json!({"type": 3, "message": "Ready."}),
            ),
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "python",
                serde_json::json!({"type": 3, "message": "Done."}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 2, 3, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("info"), "should contain info label: {text}");
        assert!(text.contains("3 messages"), "should contain count: {text}");

        let plain = format_collapsed_plain(&messages, 0, 2, 3);
        assert!(plain.contains("info"), "plain should contain info: {plain}");
        assert!(
            plain.contains("[python]"),
            "plain should contain server: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_generic_fallback() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "workspace/configuration",
                "rust-analyzer",
                serde_json::json!({"id": 1}),
            ),
            make_message_with_payload(
                "lsp",
                "workspace/configuration",
                "rust-analyzer",
                serde_json::json!({"id": 2}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("2 messages"),
            "should contain message count: {text}"
        );
        assert!(
            text.contains("workspace/configuration"),
            "should contain method: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("2 messages"),
            "plain should contain count: {plain}"
        );
        assert!(
            plain.contains("workspace/configuration"),
            "plain should contain method: {plain}"
        );
    }

    // ── Icon presence / absence tests ───────────────────────────────────

    #[test]
    fn test_format_collapsed_progress_has_icon() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "ra/indexing",
                    "value": {"kind": "begin", "title": "Indexing"}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "ra/indexing",
                    "value": {"kind": "end"}
                }),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2726}"),
            "progress run should contain progress icon (✦): {text}"
        );
    }

    #[test]
    fn test_format_collapsed_lifecycle_has_icon() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message("lsp", "initialize", "rust-analyzer"),
            make_message("lsp", "initialized", "rust-analyzer"),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{25CF}"),
            "lifecycle run should contain session_started icon (●): {text}"
        );
    }

    #[test]
    fn test_format_collapsed_mcp_init_has_icon() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message("mcp", "initialize", "catenary"),
            make_message("mcp", "notifications/initialized", "catenary"),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{25CF}"),
            "MCP init run should contain session_started icon (●): {text}"
        );
    }

    #[test]
    fn test_format_collapsed_log_info_has_icon() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "rust-analyzer",
                serde_json::json!({"type": 3, "message": "Loading..."}),
            ),
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "rust-analyzer",
                serde_json::json!({"type": 3, "message": "Ready."}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{25A2}"),
            "info log run should contain log_info icon (▢): {text}"
        );
    }

    #[test]
    fn test_format_collapsed_sync_no_icon() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "textDocument/didOpen",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
            make_message_with_payload(
                "lsp",
                "textDocument/didSave",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let first_content_span = line
            .spans
            .iter()
            .find(|s| !s.content.trim().is_empty() && s.style != theme.timestamp)
            .expect("should have a non-timestamp span");
        assert!(
            !first_content_span.content.starts_with('\u{2726}')
                && !first_content_span.content.starts_with('\u{25CF}')
                && !first_content_span.content.starts_with('\u{25A2}'),
            "sync run should not start with an icon: {:?}",
            first_content_span.content
        );
    }

    #[test]
    fn test_format_collapsed_generic_no_icon() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "workspace/configuration",
                "rust-analyzer",
                serde_json::json!({"id": 1}),
            ),
            make_message_with_payload(
                "lsp",
                "workspace/configuration",
                "rust-analyzer",
                serde_json::json!({"id": 2}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let first_content_span = line
            .spans
            .iter()
            .find(|s| !s.content.trim().is_empty() && s.style != theme.timestamp)
            .expect("should have a non-timestamp span");
        assert!(
            !first_content_span.content.starts_with('\u{2726}')
                && !first_content_span.content.starts_with('\u{25CF}')
                && !first_content_span.content.starts_with('\u{25A2}'),
            "generic run should not start with an icon: {:?}",
            first_content_span.content
        );
    }

    #[test]
    fn test_format_message_styled_no_arrow() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message("lsp", "textDocument/definition", "rust-analyzer");
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains('\u{2192}') && !text.contains('\u{2190}'),
            "LSP single should have no arrow: {text}"
        );
    }

    // ── Pair formatter tests ────────────────────────────────────────────

    #[test]
    fn test_format_pair_lsp_error_with_message() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message("lsp", "workspace/diagnostic/refresh", "rust-analyzer");
        let response = make_message_with_payload(
            "lsp",
            "workspace/diagnostic/refresh",
            "rust-analyzer",
            serde_json::json!({"error": {"code": -32601, "message": "Method not found"}}),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2718}"),
            "LSP error should show ✘ icon: {text}"
        );
        assert!(
            text.contains("[rust-analyzer]"),
            "LSP error should show server name: {text}"
        );
        assert!(
            text.contains("Method not found"),
            "should contain error message: {text}"
        );
    }

    #[test]
    fn test_format_pair_lsp_cancelled() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let response = make_message("lsp", "notifications/cancelled", "rust-analyzer");
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[rust-analyzer]"),
            "LSP cancelled should show server name: {text}"
        );
        assert!(text.contains("cancelled"), "should show cancelled: {text}");

        let plain = format_pair_plain(&request, &response);
        assert!(
            plain.contains("[rust-analyzer]"),
            "plain LSP cancelled should show server name: {plain}"
        );
    }

    #[test]
    fn test_format_pair_mcp_non_tool_error() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message("mcp", "resources/list", "catenary");
        let response = make_message_with_payload(
            "mcp",
            "resources/list",
            "catenary",
            serde_json::json!({"error": {"code": -32601, "message": "Not supported"}}),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[mcp]"),
            "MCP non-tool error should show [mcp] prefix: {text}"
        );
    }

    #[test]
    fn test_format_pair_mcp_tool_error() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": "invalid pattern", "isError": true}]}}),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2718}"),
            "MCP tool error should show ✘ icon: {text}"
        );
        assert!(
            text.contains("invalid pattern"),
            "should contain error text: {text}"
        );
        assert!(text.contains("grep"), "should contain tool name: {text}");
    }

    #[test]
    fn test_format_pair_mcp_tool_success() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": "results"}]}}),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2B9E}"),
            "MCP tool success should show tool icon ⮞, not proto_ok: {text}"
        );
        assert!(
            !text.contains("\u{2714}"),
            "MCP tool success should not show ✔ proto_ok: {text}"
        );
        assert!(text.contains("grep"), "should contain tool name: {text}");
    }

    #[test]
    fn test_format_pair_cancelled() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message("mcp", "notifications/cancelled", "catenary");
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2501}"),
            "cancellation should show ━ icon: {text}"
        );
        assert!(
            text.contains("cancelled"),
            "cancellation should show cancelled text: {text}"
        );
        assert!(
            !text.contains("x->"),
            "should not contain x-> arrow: {text}"
        );
    }

    #[test]
    fn test_format_pair_plain_no_arrows() {
        let request = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let mut response = make_message("lsp", "textDocument/hover", "rust-analyzer");
        response.payload = serde_json::json!({"result": null});
        let plain = format_pair_plain(&request, &response);
        assert!(
            !plain.contains("<->") && !plain.contains("x->"),
            "plain pair should not contain arrows: {plain}"
        );

        let cancel_response = make_message("mcp", "notifications/cancelled", "catenary");
        let cancel_plain = format_pair_plain(&request, &cancel_response);
        assert!(
            !cancel_plain.contains("<->") && !cancel_plain.contains("x->"),
            "plain cancelled pair should not contain arrows: {cancel_plain}"
        );
    }

    #[test]
    fn test_extract_jsonrpc_error() {
        let payload = serde_json::json!({"error": {"code": -32601, "message": "Method not found"}});
        assert_eq!(
            extract_jsonrpc_error(&payload).as_deref(),
            Some("Method not found")
        );

        let no_error = serde_json::json!({"result": null});
        assert_eq!(extract_jsonrpc_error(&no_error), None);

        let no_message = serde_json::json!({"error": {"code": -32601}});
        assert_eq!(extract_jsonrpc_error(&no_message), None);
    }

    #[test]
    fn test_extract_tool_error() {
        let payload = serde_json::json!({
            "result": {"content": [{"type": "text", "text": "bad pattern", "isError": true}]}
        });
        assert_eq!(extract_tool_error(&payload).as_deref(), Some("bad pattern"));

        let success = serde_json::json!({
            "result": {"content": [{"type": "text", "text": "results"}]}
        });
        assert_eq!(extract_tool_error(&success), None);

        let empty = serde_json::json!({"result": {"content": []}});
        assert_eq!(extract_tool_error(&empty), None);
    }

    // ── Scope formatter tests ─────────────────────────────────────────

    #[test]
    fn test_format_scope_styled_error() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": "bad pattern", "isError": true}]}}),
        );
        let messages = vec![request, response];
        let parent = DisplayEntry::Paired {
            request_index: 0,
            response_index: 1,
            parent_id: None,
        };
        let line =
            format_scope_styled(&parent, 5, SegmentPosition::Only, &messages, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2718}"),
            "scope with error should show ✘ icon: {text}"
        );
        assert!(
            text.contains("bad pattern"),
            "scope with error should show error message: {text}"
        );
        assert!(
            text.contains("5 children"),
            "scope should show child count: {text}"
        );
    }

    #[test]
    fn test_format_scope_styled_success() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": "results"}]}}),
        );
        let messages = vec![request, response];
        let parent = DisplayEntry::Paired {
            request_index: 0,
            response_index: 1,
            parent_id: None,
        };
        let line =
            format_scope_styled(&parent, 3, SegmentPosition::Only, &messages, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2B9E}"),
            "scope success should show tool icon ⮞: {text}"
        );
        assert!(
            !text.contains("\u{2718}"),
            "scope success should not show error icon: {text}"
        );
    }

    #[test]
    fn test_format_scope_styled_cancelled() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message("mcp", "notifications/cancelled", "catenary");
        let messages = vec![request, response];
        let parent = DisplayEntry::Paired {
            request_index: 0,
            response_index: 1,
            parent_id: None,
        };
        let line =
            format_scope_styled(&parent, 2, SegmentPosition::Only, &messages, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2501}"),
            "scope cancelled should show ━ icon: {text}"
        );
        assert!(
            text.contains("cancelled"),
            "scope cancelled should show cancelled text: {text}"
        );
    }

    #[test]
    fn test_format_pair_zero_results_not_error() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": ""}]}}),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2B9E}"),
            "zero results should show tool icon ⮞: {text}"
        );
        assert!(
            !text.contains("\u{2718}"),
            "zero results should not show error icon ✘: {text}"
        );
    }

    // ── Line count extraction tests ────────────────────────────────────

    #[test]
    fn test_extract_line_count_text() {
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [{"type": "text", "text": "a\nb\nc\nd\ne"}]}
            }),
        );
        assert_eq!(extract_line_count(&msg), Some(5));
    }

    #[test]
    fn test_extract_line_count_empty() {
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [{"type": "text", "text": ""}]}
            }),
        );
        assert_eq!(extract_line_count(&msg), Some(0));
    }

    #[test]
    fn test_extract_line_count_no_content() {
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        assert_eq!(extract_line_count(&msg), None);
    }

    #[test]
    fn test_extract_line_count_multi_content() {
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [
                    {"type": "text", "text": "a\nb\nc"},
                    {"type": "text", "text": "d\ne"}
                ]}
            }),
        );
        assert_eq!(extract_line_count(&msg), Some(5));
    }

    // ── Tool arguments extraction tests ────────────────────────────────

    #[test]
    fn test_extract_tool_arguments() {
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "params": {
                    "name": "grep",
                    "arguments": {"pattern": "foo", "glob": "**/*.rs"}
                }
            }),
        );
        let args = extract_tool_arguments(&msg).expect("should extract arguments");
        assert!(
            args.contains("pattern: \"foo\""),
            "should contain pattern: {args}"
        );
        assert!(
            args.contains("glob: \"**/*.rs\""),
            "should contain glob: {args}"
        );
        assert!(
            args.starts_with('{') && args.ends_with('}'),
            "should be wrapped in braces: {args}"
        );
    }

    #[test]
    fn test_extract_tool_arguments_none() {
        let msg = make_message_with_payload(
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            serde_json::json!({"id": 1}),
        );
        assert_eq!(extract_tool_arguments(&msg), None);
    }

    // ── Pair formatter metrics tests ───────────────────────────────────

    #[test]
    fn test_format_pair_with_metrics() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [{"type": "text", "text": "a\nb\nc\nd\ne"}]}
            }),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("5 lines"),
            "should contain line count: {text}"
        );
        assert!(text.contains('s'), "should contain timing: {text}");
    }

    #[test]
    fn test_format_pair_with_arguments() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "params": {
                    "name": "grep",
                    "arguments": {"pattern": "foo"}
                }
            }),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [{"type": "text", "text": "results"}]}
            }),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("{pattern: \"foo\"}"),
            "should contain arguments block: {text}"
        );
        // Arguments should be in a muted-styled span.
        let args_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("pattern"))
            .expect("should have an arguments span");
        assert_eq!(
            args_span.style, theme.muted,
            "arguments should use muted style"
        );
    }

    #[test]
    fn test_format_pair_zero_lines() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [{"type": "text", "text": ""}]}
            }),
        );
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("0 lines"),
            "zero lines should show '0 lines': {text}"
        );
        assert!(
            !text.contains("\u{2718}"),
            "zero lines should not show error icon: {text}"
        );
    }

    #[test]
    fn test_format_pair_cancelled_no_metrics() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message("mcp", "notifications/cancelled", "catenary");
        let line = format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("cancelled"), "should show cancelled: {text}");
        assert!(
            !text.contains("lines"),
            "cancelled should not show line count: {text}"
        );
    }

    // ── Scope formatter metrics tests ──────────────────────────────────

    #[test]
    fn test_format_scope_last_segment_metrics() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep", "arguments": {"pattern": "foo"}}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [{"type": "text", "text": "a\nb\nc"}]}
            }),
        );
        let messages = vec![request, response];
        let parent = DisplayEntry::Paired {
            request_index: 0,
            response_index: 1,
            parent_id: None,
        };

        // Last segment should show metrics.
        let last =
            format_scope_styled(&parent, 5, SegmentPosition::Last, &messages, &icons, &theme);
        let last_text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            last_text.contains("3 lines"),
            "Last segment should show line count: {last_text}"
        );
        assert!(
            last_text.contains("pattern"),
            "Last segment should show arguments: {last_text}"
        );

        // First segment should not show line count.
        let first = format_scope_styled(
            &parent,
            5,
            SegmentPosition::First,
            &messages,
            &icons,
            &theme,
        );
        let first_text: String = first.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !first_text.contains("lines"),
            "First segment should not show line count: {first_text}"
        );
        assert!(
            first_text.contains("pattern"),
            "First segment should still show arguments: {first_text}"
        );
    }

    // ── Scope plain formatter tests ───────────────────────────────────

    #[test]
    fn test_format_scope_plain_mcp_tool() {
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep", "arguments": {"pattern": "foo"}}}),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": "a\nb\nc"}]}}),
        );
        let messages = vec![request, response];
        let parent = DisplayEntry::Paired {
            request_index: 0,
            response_index: 1,
            parent_id: None,
        };
        let plain = format_scope_plain(&parent, 5, SegmentPosition::Only, &messages);
        assert!(
            plain.contains("grep"),
            "MCP tool scope should show tool name: {plain}"
        );
        assert!(
            plain.contains("3 lines"),
            "Only position should show line count: {plain}"
        );
        assert!(
            plain.contains("5 children"),
            "should show child count: {plain}"
        );
    }

    #[test]
    fn test_format_scope_plain_lsp() {
        let request = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let mut response = make_message("lsp", "textDocument/hover", "rust-analyzer");
        response.payload = serde_json::json!({"result": null});
        let messages = vec![request, response];
        let parent = DisplayEntry::Paired {
            request_index: 0,
            response_index: 1,
            parent_id: None,
        };
        let plain = format_scope_plain(&parent, 3, SegmentPosition::Only, &messages);
        assert!(
            plain.contains("[rust-analyzer]"),
            "LSP scope should show server name: {plain}"
        );
        assert!(
            plain.contains("textDocument/hover"),
            "LSP scope should show method: {plain}"
        );
    }

    // ── Plain formatter metrics tests ──────────────────────────────────

    #[test]
    fn test_format_pair_plain_with_metrics() {
        let request = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "params": {
                    "name": "grep",
                    "arguments": {"pattern": "foo", "glob": "**/*.rs"}
                }
            }),
        );
        let response = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({
                "result": {"content": [{"type": "text", "text": "a\nb\nc"}]}
            }),
        );
        let plain = format_pair_plain(&request, &response);
        assert!(
            plain.contains("3 lines"),
            "plain should contain line count: {plain}"
        );
        assert!(
            plain.contains("pattern: \"foo\""),
            "plain should contain arguments: {plain}"
        );
    }
}
