// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Extractor functions for LSP response and notification fields.
//!
//! One function per field or concept Catenary inspects. Each reads from
//! a `serde_json::Value` — no `lsp_types` dependency.

use serde_json::Value;

use super::types::{Position, Range};

// ── Private helpers ─────────────────────────────────────────────────

/// Safely converts a JSON unsigned integer to `u32`.
fn as_u32(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

// ── Server capabilities (from InitializeResult.capabilities) ────────

/// Returns whether the server supports dynamic workspace folder changes.
///
/// Requires both `workspace.workspaceFolders.supported: true` and
/// a truthy `changeNotifications` (either `true` or a registration ID string).
#[must_use]
pub fn supports_workspace_folders(caps: &Value) -> bool {
    let wf = caps
        .get("workspace")
        .and_then(|w| w.get("workspaceFolders"));
    let Some(wf) = wf else { return false };

    let supported = wf
        .get("supported")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let accepts_changes = wf
        .get("changeNotifications")
        .is_some_and(|cn| cn.as_bool() == Some(true) || cn.is_string());

    supported && accepts_changes
}

/// Returns whether the server wants `textDocument/didSave` notifications.
///
/// Short-form `textDocumentSync` (non-zero kind) implies save support.
/// Long-form checks for the presence of the `save` field.
#[must_use]
pub fn wants_did_save(caps: &Value) -> bool {
    match caps.get("textDocumentSync") {
        Some(v) if v.is_number() => v.as_u64().and_then(|n| u8::try_from(n).ok()).unwrap_or(0) != 0,
        Some(v) if v.is_object() => v.get("save").is_some_and(|s| !s.is_null()),
        _ => false,
    }
}

/// Extracts the `textDocumentSync` change kind (0=None, 1=Full, 2=Incremental).
///
/// Handles both short-form (bare number) and long-form (`{ change: N }`).
/// Returns 0 (None) when absent or unparseable.
// Wired up by Phase 1 (pull timing reliability)
#[must_use]
#[allow(dead_code, reason = "Phase 1 — pull timing reliability")]
pub fn text_document_sync_kind(caps: &Value) -> u8 {
    match caps.get("textDocumentSync") {
        Some(v) if v.is_number() => v.as_u64().and_then(|n| u8::try_from(n).ok()).unwrap_or(0),
        Some(v) => v
            .get("change")
            .and_then(Value::as_u64)
            .and_then(|n| u8::try_from(n).ok())
            .unwrap_or(0),
        None => 0,
    }
}

/// Extracts the negotiated `positionEncoding` from server capabilities.
#[must_use]
pub fn position_encoding(caps: &Value) -> Option<&str> {
    caps.get("positionEncoding")?.as_str()
}

// ── InitializeResult (full result, not just capabilities) ───────────

/// Extracts the server version string from `serverInfo.version`.
#[must_use]
pub fn server_version(result: &Value) -> Option<&str> {
    result.get("serverInfo")?.get("version")?.as_str()
}

// ── publishDiagnostics notification params ──────────────────────────

/// Extracts the document URI from `publishDiagnostics` params.
#[must_use]
pub fn publish_diagnostics_uri(params: &Value) -> Option<&str> {
    params.get("uri")?.as_str()
}

/// Extracts the document version from `publishDiagnostics` params.
#[must_use]
pub fn publish_diagnostics_version(params: &Value) -> Option<i32> {
    params
        .get("version")?
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
}

/// Extracts the diagnostics array from `publishDiagnostics` params.
///
/// Returns an empty `Vec` if the field is missing or not an array.
#[must_use]
pub fn publish_diagnostics_diagnostics(params: &Value) -> Vec<Value> {
    params
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ── $/progress notification params ──────────────────────────────────

/// Extracts the progress token (string or integer) from `$/progress` params.
#[must_use]
pub fn progress_token(params: &Value) -> Option<&Value> {
    params.get("token")
}

// ── DocumentDiagnosticReport (pull diagnostics response) ────────────

/// Extracts diagnostics from a `DocumentDiagnosticReport`.
///
/// Returns the `items` array for a "full" report, or an empty
/// `Vec` for "unchanged" or unrecognized responses.
#[must_use]
pub fn document_diagnostic_report(result: &Value) -> Vec<Value> {
    match result.get("kind").and_then(Value::as_str) {
        Some("full") => result
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Extracts per-file diagnostics from a `WorkspaceDiagnosticReport`.
///
/// Returns a `(uri, diagnostics)` pair for every `full` document report,
/// **including clean ones** (empty `items`) — the caller needs the clean set to
/// render the `[clean]` receipt (workstream 37 ticket 04). An `unchanged`
/// report carries no new diagnostics (the client's cached result stands) and is
/// skipped; a malformed entry (missing `uri`) is skipped.
#[must_use]
pub fn workspace_diagnostic_report(result: &Value) -> Vec<(String, Vec<Value>)> {
    let Some(items) = result.get("items").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    for item in items {
        let Some(uri) = item.get("uri").and_then(Value::as_str) else {
            continue;
        };
        if item.get("kind").and_then(Value::as_str) == Some("full") {
            let diags = item
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            reports.push((uri.to_string(), diags));
        }
    }
    reports
}

// ── Individual diagnostic fields ────────────────────────────────────

/// Extracts the severity from a diagnostic (1=Error, 2=Warning, 3=Info, 4=Hint).
#[must_use]
pub fn diagnostic_severity(diag: &Value) -> Option<u8> {
    diag.get("severity")?
        .as_u64()
        .and_then(|v| u8::try_from(v).ok())
}

/// Extracts the message from a diagnostic.
#[must_use]
pub fn diagnostic_message(diag: &Value) -> Option<&str> {
    diag.get("message")?.as_str()
}

/// Extracts the range from a diagnostic as a local [`Range`] type.
#[must_use]
pub fn diagnostic_range(diag: &Value) -> Option<Range> {
    let range = diag.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some(Range {
        start: Position {
            line: as_u32(start.get("line")?)?,
            character: as_u32(start.get("character")?)?,
        },
        end: Position {
            line: as_u32(end.get("line")?)?,
            character: as_u32(end.get("character")?)?,
        },
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── supports_workspace_folders ──────────────────────────────────

    #[test]
    fn supports_workspace_folders_full() {
        let caps = json!({
            "workspace": {
                "workspaceFolders": {
                    "supported": true,
                    "changeNotifications": true
                }
            }
        });
        assert!(supports_workspace_folders(&caps));
    }

    #[test]
    fn supports_workspace_folders_string_notification() {
        let caps = json!({
            "workspace": {
                "workspaceFolders": {
                    "supported": true,
                    "changeNotifications": "workspace-folders-id"
                }
            }
        });
        assert!(supports_workspace_folders(&caps));
    }

    #[test]
    fn supports_workspace_folders_not_supported() {
        let caps = json!({
            "workspace": {
                "workspaceFolders": {
                    "supported": false,
                    "changeNotifications": true
                }
            }
        });
        assert!(!supports_workspace_folders(&caps));
    }

    #[test]
    fn supports_workspace_folders_no_change_notifications() {
        let caps = json!({
            "workspace": {
                "workspaceFolders": {
                    "supported": true
                }
            }
        });
        assert!(!supports_workspace_folders(&caps));
    }

    #[test]
    fn supports_workspace_folders_change_notifications_false() {
        let caps = json!({
            "workspace": {
                "workspaceFolders": {
                    "supported": true,
                    "changeNotifications": false
                }
            }
        });
        assert!(!supports_workspace_folders(&caps));
    }

    #[test]
    fn supports_workspace_folders_missing() {
        assert!(!supports_workspace_folders(&json!({})));
    }

    // ── wants_did_save ──────────────────────────────────────────────

    #[test]
    fn wants_did_save_short_form_full() {
        let caps = json!({ "textDocumentSync": 1 });
        assert!(wants_did_save(&caps));
    }

    #[test]
    fn wants_did_save_short_form_incremental() {
        let caps = json!({ "textDocumentSync": 2 });
        assert!(wants_did_save(&caps));
    }

    #[test]
    fn wants_did_save_short_form_none() {
        let caps = json!({ "textDocumentSync": 0 });
        assert!(!wants_did_save(&caps));
    }

    #[test]
    fn wants_did_save_long_form_present() {
        let caps = json!({ "textDocumentSync": { "save": true } });
        assert!(wants_did_save(&caps));
    }

    #[test]
    fn wants_did_save_long_form_options() {
        let caps = json!({ "textDocumentSync": { "save": { "includeText": false } } });
        assert!(wants_did_save(&caps));
    }

    #[test]
    fn wants_did_save_long_form_absent() {
        let caps = json!({ "textDocumentSync": { "change": 1 } });
        assert!(!wants_did_save(&caps));
    }

    #[test]
    fn wants_did_save_long_form_null() {
        let caps = json!({ "textDocumentSync": { "save": null } });
        assert!(!wants_did_save(&caps));
    }

    #[test]
    fn wants_did_save_missing() {
        assert!(!wants_did_save(&json!({})));
    }

    // ── text_document_sync_kind ─────────────────────────────────────

    #[test]
    fn sync_kind_short_form() {
        assert_eq!(
            text_document_sync_kind(&json!({ "textDocumentSync": 1 })),
            1
        );
        assert_eq!(
            text_document_sync_kind(&json!({ "textDocumentSync": 2 })),
            2
        );
        assert_eq!(
            text_document_sync_kind(&json!({ "textDocumentSync": 0 })),
            0
        );
    }

    #[test]
    fn sync_kind_long_form() {
        let caps = json!({ "textDocumentSync": { "change": 2 } });
        assert_eq!(text_document_sync_kind(&caps), 2);
    }

    #[test]
    fn sync_kind_long_form_missing_change() {
        let caps = json!({ "textDocumentSync": {} });
        assert_eq!(text_document_sync_kind(&caps), 0);
    }

    #[test]
    fn sync_kind_missing() {
        assert_eq!(text_document_sync_kind(&json!({})), 0);
    }

    // ── position_encoding ───────────────────────────────────────────

    #[test]
    fn position_encoding_present() {
        let caps = json!({ "positionEncoding": "utf-8" });
        assert_eq!(position_encoding(&caps), Some("utf-8"));
    }

    #[test]
    fn position_encoding_missing() {
        assert_eq!(position_encoding(&json!({})), None);
    }

    // ── server_version ──────────────────────────────────────────────

    #[test]
    fn server_version_present() {
        let result = json!({
            "capabilities": {},
            "serverInfo": { "name": "rust-analyzer", "version": "1.2.3" }
        });
        assert_eq!(server_version(&result), Some("1.2.3"));
    }

    #[test]
    fn server_version_no_version() {
        let result = json!({
            "capabilities": {},
            "serverInfo": { "name": "rust-analyzer" }
        });
        assert_eq!(server_version(&result), None);
    }

    #[test]
    fn server_version_no_server_info() {
        let result = json!({ "capabilities": {} });
        assert_eq!(server_version(&result), None);
    }

    // ── publishDiagnostics extractors ───────────────────────────────

    #[test]
    fn publish_diagnostics_uri_present() {
        let params = json!({
            "uri": "file:///foo.rs",
            "version": 1,
            "diagnostics": []
        });
        assert_eq!(publish_diagnostics_uri(&params), Some("file:///foo.rs"));
    }

    #[test]
    fn publish_diagnostics_uri_missing() {
        assert_eq!(publish_diagnostics_uri(&json!({})), None);
    }

    #[test]
    fn publish_diagnostics_version_present() {
        let params = json!({
            "uri": "file:///foo.rs",
            "version": 42,
            "diagnostics": []
        });
        assert_eq!(publish_diagnostics_version(&params), Some(42));
    }

    #[test]
    fn publish_diagnostics_version_missing() {
        let params = json!({
            "uri": "file:///foo.rs",
            "diagnostics": []
        });
        assert_eq!(publish_diagnostics_version(&params), None);
    }

    #[test]
    fn publish_diagnostics_version_null() {
        assert_eq!(
            publish_diagnostics_version(&json!({ "version": null })),
            None
        );
    }

    #[test]
    fn publish_diagnostics_diagnostics_present() {
        let params = json!({
            "uri": "file:///foo.rs",
            "diagnostics": [{
                "message": "unused variable",
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                }
            }]
        });
        let diags = publish_diagnostics_diagnostics(&params);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].get("message").and_then(Value::as_str),
            Some("unused variable")
        );
    }

    #[test]
    fn publish_diagnostics_diagnostics_empty() {
        let params = json!({ "diagnostics": [] });
        assert!(publish_diagnostics_diagnostics(&params).is_empty());
    }

    #[test]
    fn publish_diagnostics_diagnostics_missing() {
        assert!(publish_diagnostics_diagnostics(&json!({})).is_empty());
    }

    // ── Progress extractors ─────────────────────────────────────────

    #[test]
    fn progress_token_string() {
        let params = json!({
            "token": "rustAnalyzer/flycheck",
            "value": { "kind": "begin", "title": "Checking" }
        });
        assert_eq!(
            progress_token(&params).and_then(Value::as_str),
            Some("rustAnalyzer/flycheck")
        );
    }

    #[test]
    fn progress_token_number() {
        let params = json!({
            "token": 42,
            "value": { "kind": "end" }
        });
        assert_eq!(progress_token(&params).and_then(Value::as_i64), Some(42));
    }

    #[test]
    fn progress_token_missing() {
        assert!(progress_token(&json!({})).is_none());
    }

    // ── DocumentDiagnosticReport extractors ──────────────────────────

    #[test]
    fn document_diagnostic_report_full() {
        let result = json!({
            "kind": "full",
            "items": [{
                "message": "unused variable",
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                }
            }]
        });
        let diags = document_diagnostic_report(&result);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].get("message").and_then(Value::as_str),
            Some("unused variable")
        );
    }

    #[test]
    fn document_diagnostic_report_unchanged() {
        let result = json!({
            "kind": "unchanged",
            "resultId": "abc123"
        });
        assert!(document_diagnostic_report(&result).is_empty());
    }

    #[test]
    fn document_diagnostic_report_empty_items() {
        let result = json!({
            "kind": "full",
            "items": []
        });
        assert!(document_diagnostic_report(&result).is_empty());
    }

    #[test]
    fn document_diagnostic_report_missing_kind() {
        let result = json!({
            "items": [{ "message": "err" }]
        });
        assert!(document_diagnostic_report(&result).is_empty());
    }

    // ── WorkspaceDiagnosticReport extractor ──────────────────────────

    #[test]
    fn workspace_diagnostic_report_mixed_full_reports() {
        // Dirty and clean documents both arrive as `full` reports; the clean one
        // (empty items) is retained so the receipt can render/collapse it.
        let result = json!({
            "items": [
                {
                    "uri": "file:///p/dirty.rs",
                    "kind": "full",
                    "items": [{ "message": "boom", "severity": 1 }]
                },
                {
                    "uri": "file:///p/clean.rs",
                    "kind": "full",
                    "items": []
                }
            ]
        });
        let reports = workspace_diagnostic_report(&result);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].0, "file:///p/dirty.rs");
        assert_eq!(reports[0].1.len(), 1);
        assert_eq!(reports[1].0, "file:///p/clean.rs");
        assert!(reports[1].1.is_empty());
    }

    #[test]
    fn workspace_diagnostic_report_skips_unchanged_and_malformed() {
        // `unchanged` reports carry no new diagnostics; a report missing `uri`
        // is malformed. Both are dropped.
        let result = json!({
            "items": [
                { "uri": "file:///p/a.rs", "kind": "unchanged", "resultId": "x" },
                { "kind": "full", "items": [{ "message": "no uri" }] },
                { "uri": "file:///p/b.rs", "kind": "full", "items": [] }
            ]
        });
        let reports = workspace_diagnostic_report(&result);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, "file:///p/b.rs");
    }

    #[test]
    fn workspace_diagnostic_report_no_items_is_empty() {
        assert!(workspace_diagnostic_report(&json!({})).is_empty());
    }

    // ── Diagnostic extractors ───────────────────────────────────────

    #[test]
    fn diagnostic_severity_present() {
        let diag = json!({ "severity": 1, "message": "err", "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 0 }
        }});
        assert_eq!(diagnostic_severity(&diag), Some(1));
    }

    #[test]
    fn diagnostic_severity_warning() {
        let diag = json!({ "severity": 2, "message": "warn", "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 0 }
        }});
        assert_eq!(diagnostic_severity(&diag), Some(2));
    }

    #[test]
    fn diagnostic_severity_missing() {
        assert_eq!(diagnostic_severity(&json!({})), None);
    }

    #[test]
    fn diagnostic_severity_null() {
        assert_eq!(diagnostic_severity(&json!({ "severity": null })), None);
    }

    #[test]
    fn diagnostic_severity_wrong_type() {
        assert_eq!(diagnostic_severity(&json!({ "severity": "error" })), None);
    }

    #[test]
    fn diagnostic_message_present() {
        let diag = json!({ "message": "unused variable", "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 0 }
        }});
        assert_eq!(diagnostic_message(&diag), Some("unused variable"));
    }

    #[test]
    fn diagnostic_message_missing() {
        assert_eq!(diagnostic_message(&json!({})), None);
    }

    #[test]
    fn diagnostic_range_present() {
        let diag = json!({
            "message": "err",
            "range": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 1, "character": 10 }
            }
        });
        assert_eq!(
            diagnostic_range(&diag),
            Some(Range {
                start: Position {
                    line: 1,
                    character: 2
                },
                end: Position {
                    line: 1,
                    character: 10
                },
            })
        );
    }

    #[test]
    fn diagnostic_range_missing() {
        assert_eq!(diagnostic_range(&json!({})), None);
    }

    #[test]
    fn diagnostic_range_partial() {
        // Missing end position
        let diag = json!({
            "range": {
                "start": { "line": 0, "character": 0 }
            }
        });
        assert_eq!(diagnostic_range(&diag), None);
    }

    #[test]
    fn diagnostic_range_wrong_type() {
        let diag = json!({
            "range": {
                "start": { "line": "zero", "character": 0 },
                "end": { "line": 0, "character": 0 }
            }
        });
        assert_eq!(diagnostic_range(&diag), None);
    }
}
