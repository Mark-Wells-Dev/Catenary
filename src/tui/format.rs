// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Message formatting helpers for the TUI.
//!
//! Styled and plain-text formatters for single messages and scope headers.

use ratatui::text::{Line, Span};

use super::icons::{IconSet, basename, diag_style, tool_icon};
use super::scope::{Scope, ScopeState};
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
/// records. Icons appear only on scope headers where they serve as
/// at-a-glance status signals.
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

// ── Scope lifecycle header ──────────────────────────────────────────────

/// Build a styled [`Line`] for a scope header.
///
/// Renders from the scope's request message. Closed scopes with a
/// response show outcome icon, timing, and line count. Open scopes
/// show an activity indicator.
#[must_use]
pub fn format_scope_header_styled(scope: &Scope, icons: &IconSet, theme: &Theme) -> Line<'static> {
    let header = scope.header_message();
    let ts = header.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    let child_count = scope.child_count();
    let children_label = format!(
        "{child_count} child{}",
        if child_count == 1 { "" } else { "ren" }
    );

    // Extract tool name from MCP request payload.
    let tool_name = Some(&scope.request)
        .filter(|r| r.method == "tools/call")
        .and_then(|r| r.payload.get("params"))
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());

    let label = tool_name.unwrap_or(&header.method);

    // Closed scopes with a response get outcome-aware rendering.
    if scope.state == ScopeState::Closed
        && let Some(resp) = scope.response.as_ref()
    {
        let delta_ms = resp
            .timestamp
            .signed_duration_since(scope.request.timestamp)
            .num_milliseconds();
        let timing = format_duration_short(delta_ms);
        let outcome = pair_outcome(resp);

        let (icon, icon_style, name_text, meta) = match &outcome {
            PairOutcome::Cancelled => {
                let meta = format!(" (cancelled, {children_label}, {timing})");
                (
                    icons.cancelled.clone(),
                    theme.muted,
                    label.to_string(),
                    meta,
                )
            }
            PairOutcome::Error { message } => {
                let error_suffix = message
                    .as_deref()
                    .map_or(String::new(), |m| format!(": {m}"));
                let meta = format!(" ({children_label}, {timing})");
                (
                    icons.proto_error.clone(),
                    theme.error,
                    format!("{label}{error_suffix}"),
                    meta,
                )
            }
            PairOutcome::Success => {
                let line_count = tool_name.and_then(|_| extract_line_count(resp));
                let metrics = format_tool_metrics(line_count, &timing);
                let meta = format!(" ({metrics}, {children_label})");
                let icon = tool_name.map_or_else(
                    || icons.proto_ok.clone(),
                    |tn| tool_icon(tn, icons).to_string(),
                );
                (icon, theme.success, label.to_string(), meta)
            }
        };

        let args = tool_name.and_then(|_| extract_tool_arguments(&scope.request));

        let mut spans = vec![ts_span, Span::styled(icon, icon_style)];
        spans.push(Span::styled(name_text, theme.text));
        spans.push(Span::styled(meta, theme.muted));
        if let Some(args_str) = args {
            spans.push(Span::styled(format!(" {args_str}"), theme.muted));
        }
        return Line::from(spans);
    }

    // Open scope: activity indicator.
    let icon = tool_name.map_or_else(
        || icons.tool_default.clone(),
        |tn| tool_icon(tn, icons).to_string(),
    );

    let args = tool_name.and_then(|_| extract_tool_arguments(&scope.request));

    let mut spans = vec![ts_span, Span::styled(icon, theme.accent)];
    spans.push(Span::styled(label.to_string(), theme.text));
    spans.push(Span::styled(format!(" ({children_label})"), theme.muted));
    if let Some(args_str) = args {
        spans.push(Span::styled(format!(" {args_str}"), theme.muted));
    }
    Line::from(spans)
}

/// Plain-text scope header (used for yank/clipboard).
#[must_use]
pub fn format_scope_header_plain(scope: &Scope, icons: &IconSet) -> String {
    let header = scope.header_message();
    let ts = header.timestamp.format("%H:%M:%S");

    let tool_name = Some(&scope.request)
        .filter(|r| r.method == "tools/call")
        .and_then(|r| r.payload.get("params"))
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());

    let label = tool_name.unwrap_or(&header.method);

    if scope.state == ScopeState::Closed
        && let Some(resp) = scope.response.as_ref()
    {
        let delta_ms = resp
            .timestamp
            .signed_duration_since(scope.request.timestamp)
            .num_milliseconds();
        let timing = format_duration_short(delta_ms);
        let outcome = pair_outcome(resp);

        let status = match &outcome {
            PairOutcome::Cancelled => "cancelled".to_string(),
            PairOutcome::Error { message } => message
                .as_deref()
                .map_or_else(|| "error".to_string(), |m| format!("error: {m}")),
            PairOutcome::Success => {
                let line_count = tool_name.and_then(|_| extract_line_count(resp));
                format_tool_metrics(line_count, &timing)
            }
        };

        let args = tool_name.and_then(|_| extract_tool_arguments(&scope.request));
        let icon = tool_name.map_or_else(String::new, |tn| tool_icon(tn, icons).to_string());
        args.map_or_else(
            || format!("{ts} {icon}{label} ({status})"),
            |a| format!("{ts} {icon}{label} ({status}) {a}"),
        )
    } else {
        let args = tool_name.and_then(|_| extract_tool_arguments(&scope.request));
        let icon = tool_name.map_or_else(String::new, |tn| tool_icon(tn, icons).to_string());
        args.map_or_else(
            || format!("{ts} {icon}{label}"),
            |a| format!("{ts} {icon}{label} {a}"),
        )
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
        let ts = Utc::now() - TimeDelta::seconds(60);
        assert_eq!(format_ago(ts), "1m ago");
        let ts = Utc::now() - TimeDelta::seconds(3600);
        assert_eq!(format_ago(ts), "1h ago");
        let ts = Utc::now() - TimeDelta::seconds(86400);
        assert_eq!(format_ago(ts), "1d ago");
    }

    #[test]
    fn test_format_duration_short() {
        assert_eq!(format_duration_short(0), "0.0s");
        assert_eq!(format_duration_short(500), "0.5s");
        assert_eq!(format_duration_short(3200), "3.2s");
        assert_eq!(format_duration_short(9999), "10.0s");
        assert_eq!(format_duration_short(10_000), "10s");
        assert_eq!(format_duration_short(45_000), "45s");
        assert_eq!(format_duration_short(-100), "0.0s");
    }

    #[test]
    fn test_format_message_styled_lsp() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("[rust-analyzer]"));
        assert!(text.contains("textDocument/hover"));
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
            serde_json::json!({"file": "/src/lib.rs", "count": 2, "preview": "\t:12:1 [error] rustc: bad"}),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("lib.rs"));
        assert!(text.contains("2 diagnostics"));
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
        assert!(line.spans.iter().any(|s| s.style == theme.success));
    }

    #[test]
    fn test_format_message_plain() {
        let msg = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let plain = format_message_plain(&msg);
        assert!(plain.contains("[rust-analyzer]"));
        assert!(plain.contains("textDocument/hover"));
    }

    #[test]
    fn test_format_message_progress_begin() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({"token": "wid/1", "value": {"kind": "begin", "title": "Indexing", "percentage": 0}}),
        );
        let styled = format_message_styled(&msg, &icons, &theme);
        let text: String = styled.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Indexing"));
        assert!(text.contains("0%"));

        let plain = format_message_plain(&msg);
        assert!(plain.contains("Indexing"));
        assert!(plain.contains("0%"));
    }

    #[test]
    fn test_format_message_progress_end() {
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({"token": "wid/1", "value": {"kind": "end"}}),
        );
        let plain = format_message_plain(&msg);
        assert!(plain.contains("done"));
    }

    #[test]
    fn test_progress_suffix_bare_report() {
        let payload = serde_json::json!({"value": {"kind": "report"}});
        assert_eq!(progress_suffix(&payload), None);
    }

    #[test]
    fn test_extract_jsonrpc_error() {
        let payload = serde_json::json!({"error": {"code": -32601, "message": "Method not found"}});
        assert_eq!(
            extract_jsonrpc_error(&payload).as_deref(),
            Some("Method not found")
        );
        assert_eq!(
            extract_jsonrpc_error(&serde_json::json!({"result": null})),
            None
        );
    }

    #[test]
    fn test_extract_tool_error() {
        let payload = serde_json::json!({"result": {"content": [{"type": "text", "text": "bad pattern", "isError": true}]}});
        assert_eq!(extract_tool_error(&payload).as_deref(), Some("bad pattern"));
        assert_eq!(
            extract_tool_error(
                &serde_json::json!({"result": {"content": [{"type": "text", "text": "ok"}]}})
            ),
            None
        );
    }

    #[test]
    fn test_extract_line_count() {
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": "a\nb\nc\nd\ne"}]}}),
        );
        assert_eq!(extract_line_count(&msg), Some(5));

        let empty = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"result": {"content": [{"type": "text", "text": ""}]}}),
        );
        assert_eq!(extract_line_count(&empty), Some(0));

        let no_content = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        assert_eq!(extract_line_count(&no_content), None);
    }

    #[test]
    fn test_extract_tool_arguments() {
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep", "arguments": {"pattern": "foo", "glob": "**/*.rs"}}}),
        );
        let args = extract_tool_arguments(&msg).expect("should extract arguments");
        assert!(args.contains("pattern: \"foo\""));
        assert!(args.contains("glob: \"**/*.rs\""));
        assert!(args.starts_with('{') && args.ends_with('}'));
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
}
