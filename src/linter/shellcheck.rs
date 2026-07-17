// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! shellcheck adapter (`shellcheck -f json1`).
//!
//! Parses the `json1` document — `{"comments": [ ... ]}` — into LSP-shaped
//! diagnostics. shellcheck gives a full 1-based range (`line`/`endLine`,
//! `column`/`endColumn`), a `level`, and an integer `code` rendered as
//! `SC<code>`. `source` is always `"shellcheck"`.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{RawLinterDiag, lsp_range};

/// The `source` field stamped on every shellcheck diagnostic.
const SOURCE: &str = "shellcheck";

/// Parses `shellcheck -f json1` output into LSP-shaped diagnostics.
///
/// # Errors
///
/// Returns an error if the output is not valid JSON or lacks the `comments`
/// array the `json1` format guarantees.
pub(super) fn parse(output: &str) -> Result<Vec<RawLinterDiag>> {
    let root: Value = serde_json::from_str(output).context("shellcheck json1: invalid JSON")?;
    let comments = root
        .get("comments")
        .and_then(Value::as_array)
        .context("shellcheck json1: missing `comments` array")?;

    let mut out = Vec::with_capacity(comments.len());
    for comment in comments {
        let Some(file) = comment.get("file").and_then(Value::as_str) else {
            continue;
        };
        let line = comment.get("line").and_then(Value::as_u64).unwrap_or(1);
        let end_line = comment
            .get("endLine")
            .and_then(Value::as_u64)
            .unwrap_or(line);
        let col = comment.get("column").and_then(Value::as_u64).unwrap_or(1);
        let end_col = comment
            .get("endColumn")
            .and_then(Value::as_u64)
            .unwrap_or(col);
        let level = comment
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("warning");
        let message = comment
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // shellcheck codes are integers; render `SC<code>` so `source` + `code`
        // are always populated (dedup, ticket 02, rides on them).
        let code = comment
            .get("code")
            .and_then(Value::as_u64)
            .map_or_else(|| SOURCE.to_string(), |n| format!("SC{n}"));

        out.push(RawLinterDiag {
            file: file.to_string(),
            diagnostic: json!({
                "range": lsp_range(line, col, end_line, end_col),
                "severity": level_to_severity(level),
                "source": SOURCE,
                "code": code,
                "message": message,
            }),
        });
    }
    Ok(out)
}

/// Maps a shellcheck `level` to an LSP severity (1=Error … 4=Hint).
fn level_to_severity(level: &str) -> u8 {
    match level {
        "error" => 1,
        "info" => 3,
        "style" => 4,
        // "warning" and anything unrecognized.
        _ => 2,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// Golden `shellcheck -f json1` output for a script with two findings.
    const GOLDEN: &str = r#"{
      "comments": [
        {
          "file": "/proj/run.sh",
          "line": 3,
          "endLine": 3,
          "column": 6,
          "endColumn": 9,
          "level": "warning",
          "code": 2086,
          "message": "Double quote to prevent globbing and word splitting."
        },
        {
          "file": "/proj/run.sh",
          "line": 5,
          "endLine": 5,
          "column": 1,
          "endColumn": 4,
          "level": "error",
          "code": 1009,
          "message": "The mentioned syntax error was in this simple command."
        }
      ]
    }"#;

    #[test]
    fn parses_golden_to_lsp_diagnostics() {
        let diags = parse(GOLDEN).expect("parse golden");
        assert_eq!(diags.len(), 2);

        let first = &diags[0];
        assert_eq!(first.file, "/proj/run.sh");
        let d = &first.diagnostic;
        assert_eq!(d["source"], "shellcheck");
        assert_eq!(d["code"], "SC2086");
        assert_eq!(d["severity"], 2); // warning
        // 1-based (3,6)-(3,9) → 0-based (2,5)-(2,8).
        assert_eq!(d["range"]["start"]["line"], 2);
        assert_eq!(d["range"]["start"]["character"], 5);
        assert_eq!(d["range"]["end"]["character"], 8);
        assert_eq!(
            d["message"],
            "Double quote to prevent globbing and word splitting."
        );

        let second = &diags[1];
        assert_eq!(second.diagnostic["code"], "SC1009");
        assert_eq!(second.diagnostic["severity"], 1); // error
    }

    #[test]
    fn clean_document_yields_no_diagnostics() {
        let diags = parse(r#"{"comments": []}"#).expect("parse clean");
        assert!(diags.is_empty());
    }

    #[test]
    fn level_mapping_covers_all_bands() {
        assert_eq!(level_to_severity("error"), 1);
        assert_eq!(level_to_severity("warning"), 2);
        assert_eq!(level_to_severity("info"), 3);
        assert_eq!(level_to_severity("style"), 4);
        assert_eq!(level_to_severity("mystery"), 2);
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse("{not json").is_err());
    }

    #[test]
    fn missing_comments_array_errors() {
        assert!(parse(r#"{"other": 1}"#).is_err());
    }
}
