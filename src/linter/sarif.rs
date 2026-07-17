// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Generic SARIF adapter — one adapter for any SARIF-emitting linter.
//!
//! Ingests `runs[].results[]`: `tool.driver.name` → `source`, `ruleId` → `code`,
//! `region` → `range`, `level` → `severity`, `message.text` → `message`. A user
//! with a non-SARIF tool wraps it to emit SARIF (often a one-line `--format
//! sarif`); there is no generic errorformat engine.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{RawLinterDiag, lsp_range};

/// Parses a SARIF document into LSP-shaped diagnostics.
///
/// # Errors
///
/// Returns an error if the output is not valid JSON or lacks the `runs` array a
/// SARIF log requires.
pub(super) fn parse(output: &str) -> Result<Vec<RawLinterDiag>> {
    let root: Value = serde_json::from_str(output).context("SARIF: invalid JSON")?;
    let runs = root
        .get("runs")
        .and_then(Value::as_array)
        .context("SARIF: missing `runs` array")?;

    let mut out = Vec::new();
    for run in runs {
        let tool_name = run
            .get("tool")
            .and_then(|t| t.get("driver"))
            .and_then(|d| d.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("sarif");
        let Some(results) = run.get("results").and_then(Value::as_array) else {
            continue;
        };
        for result in results {
            if let Some(diag) = parse_result(result, tool_name) {
                out.push(diag);
            }
        }
    }
    Ok(out)
}

/// Parses a single `runs[].results[]` entry, or `None` if it has no file
/// location to anchor on.
fn parse_result(result: &Value, tool_name: &str) -> Option<RawLinterDiag> {
    let physical = result
        .get("locations")
        .and_then(Value::as_array)
        .and_then(|locs| locs.first())
        .and_then(|loc| loc.get("physicalLocation"));

    let file = physical
        .and_then(|p| p.get("artifactLocation"))
        .and_then(|a| a.get("uri"))
        .and_then(Value::as_str)?;

    let region = physical.and_then(|p| p.get("region"));
    let start_line = region
        .and_then(|r| r.get("startLine"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let start_col = region
        .and_then(|r| r.get("startColumn"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let end_line = region
        .and_then(|r| r.get("endLine"))
        .and_then(Value::as_u64)
        .unwrap_or(start_line);
    let end_col = region
        .and_then(|r| r.get("endColumn"))
        .and_then(Value::as_u64)
        .unwrap_or(start_col);

    let rule_id = result
        .get("ruleId")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .get("rule")
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default();
    // SARIF default level is "warning" when the property is absent.
    let level = result
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("warning");
    let message = result
        .get("message")
        .and_then(|m| m.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    Some(RawLinterDiag {
        file: file.to_string(),
        diagnostic: json!({
            "range": lsp_range(start_line, start_col, end_line, end_col),
            "severity": level_to_severity(level),
            "source": tool_name,
            "code": rule_id,
            "message": message,
        }),
    })
}

/// Maps a SARIF `level` to an LSP severity (1=Error … 4=Hint).
fn level_to_severity(level: &str) -> u8 {
    match level {
        "error" => 1,
        "note" => 3,
        "none" => 4,
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

    /// Golden SARIF log with two results from one tool.
    const GOLDEN: &str = r#"{
      "version": "2.1.0",
      "runs": [
        {
          "tool": { "driver": { "name": "hadolint" } },
          "results": [
            {
              "ruleId": "DL3008",
              "level": "warning",
              "message": { "text": "Pin versions in apt get install." },
              "locations": [
                {
                  "physicalLocation": {
                    "artifactLocation": { "uri": "Dockerfile" },
                    "region": { "startLine": 4, "startColumn": 1, "endLine": 4, "endColumn": 20 }
                  }
                }
              ]
            },
            {
              "ruleId": "DL4006",
              "level": "error",
              "message": { "text": "Set the SHELL option -o pipefail." },
              "locations": [
                {
                  "physicalLocation": {
                    "artifactLocation": { "uri": "Dockerfile" },
                    "region": { "startLine": 9 }
                  }
                }
              ]
            }
          ]
        }
      ]
    }"#;

    #[test]
    fn parses_golden_to_lsp_diagnostics() {
        let diags = parse(GOLDEN).expect("parse golden");
        assert_eq!(diags.len(), 2);

        let first = &diags[0];
        assert_eq!(first.file, "Dockerfile");
        let d = &first.diagnostic;
        assert_eq!(d["source"], "hadolint");
        assert_eq!(d["code"], "DL3008");
        assert_eq!(d["severity"], 2); // warning
        // 1-based (4,1)-(4,20) → 0-based (3,0)-(3,19).
        assert_eq!(d["range"]["start"]["line"], 3);
        assert_eq!(d["range"]["start"]["character"], 0);
        assert_eq!(d["range"]["end"]["character"], 19);
        assert_eq!(d["message"], "Pin versions in apt get install.");

        // A region with only startLine defaults the rest.
        let second = &diags[1];
        assert_eq!(second.diagnostic["code"], "DL4006");
        assert_eq!(second.diagnostic["severity"], 1); // error
        assert_eq!(second.diagnostic["range"]["start"]["line"], 8);
        assert_eq!(second.diagnostic["range"]["start"]["character"], 0);
        assert_eq!(second.diagnostic["range"]["end"]["line"], 8);
    }

    #[test]
    fn level_absent_defaults_to_warning() {
        let sarif = r#"{
          "runs": [{
            "tool": { "driver": { "name": "t" } },
            "results": [{
              "ruleId": "R1",
              "message": { "text": "m" },
              "locations": [{ "physicalLocation": {
                "artifactLocation": { "uri": "f" },
                "region": { "startLine": 1 }
              }}]
            }]
          }]
        }"#;
        let diags = parse(sarif).expect("parse");
        assert_eq!(diags[0].diagnostic["severity"], 2);
    }

    #[test]
    fn empty_runs_yields_no_diagnostics() {
        let diags = parse(r#"{"runs": []}"#).expect("parse");
        assert!(diags.is_empty());
    }

    #[test]
    fn missing_runs_errors() {
        assert!(parse(r#"{"version":"2.1.0"}"#).is_err());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse("not json at all").is_err());
    }
}
