// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! actionlint adapter (`actionlint -format '{{json .}}'`).
//!
//! Parses the JSON array of findings into LSP-shaped diagnostics. actionlint
//! reports a 1-based `line`/`column`, a `message`, and a `kind` (the rule
//! category) used as a coarse `code`. It carries no severity level, so every
//! finding maps to Error — actionlint only emits when a workflow is broken.
//! `source` is always `"actionlint"`.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{RawLinterDiag, lsp_range};

/// The `source` field stamped on every actionlint diagnostic.
const SOURCE: &str = "actionlint";

/// Parses `actionlint -format '{{json .}}'` output into LSP-shaped diagnostics.
///
/// # Errors
///
/// Returns an error if the output is not a valid JSON array.
pub(super) fn parse(output: &str) -> Result<Vec<RawLinterDiag>> {
    let value: Value = serde_json::from_str(output).context("actionlint json: invalid JSON")?;
    let items = value
        .as_array()
        .context("actionlint json: expected a top-level array")?;

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(file) = item.get("filepath").and_then(Value::as_str) else {
            continue;
        };
        let line = item.get("line").and_then(Value::as_u64).unwrap_or(1);
        let col = item.get("column").and_then(Value::as_u64).unwrap_or(1);
        let message = item
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // `kind` is the rule category — use it as a coarse code so `source` +
        // `code` are always populated.
        let code = item
            .get("kind")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(SOURCE);

        out.push(RawLinterDiag {
            file: file.to_string(),
            diagnostic: json!({
                // actionlint reports a point, not a span — end == start.
                "range": lsp_range(line, col, line, col),
                "severity": 1,
                "source": SOURCE,
                "code": code,
                "message": message,
            }),
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// Golden `actionlint -format '{{json .}}'` output.
    const GOLDEN: &str = r#"[
      {
        "message": "property \"runs-onn\" is not defined",
        "filepath": ".github/workflows/ci.yml",
        "line": 12,
        "column": 5,
        "kind": "syntax-check",
        "snippet": "    runs-onn: ubuntu-latest"
      },
      {
        "message": "shellcheck reported issue in this script: SC2086:info:1:6",
        "filepath": ".github/workflows/ci.yml",
        "line": 20,
        "column": 9,
        "kind": "shellcheck"
      }
    ]"#;

    #[test]
    fn parses_golden_to_lsp_diagnostics() {
        let diags = parse(GOLDEN).expect("parse golden");
        assert_eq!(diags.len(), 2);

        let first = &diags[0];
        assert_eq!(first.file, ".github/workflows/ci.yml");
        let d = &first.diagnostic;
        assert_eq!(d["source"], "actionlint");
        assert_eq!(d["code"], "syntax-check");
        assert_eq!(d["severity"], 1);
        // 1-based (12,5) → 0-based (11,4), point span.
        assert_eq!(d["range"]["start"]["line"], 11);
        assert_eq!(d["range"]["start"]["character"], 4);
        assert_eq!(d["range"]["end"]["line"], 11);
        assert_eq!(d["range"]["end"]["character"], 4);

        assert_eq!(diags[1].diagnostic["code"], "shellcheck");
    }

    #[test]
    fn clean_array_yields_no_diagnostics() {
        let diags = parse("[]").expect("parse clean");
        assert!(diags.is_empty());
    }

    #[test]
    fn missing_kind_falls_back_to_source_code() {
        let diags =
            parse(r#"[{"message":"x","filepath":"a.yml","line":1,"column":1}]"#).expect("parse");
        assert_eq!(diags[0].diagnostic["code"], "actionlint");
    }

    #[test]
    fn non_array_errors() {
        assert!(parse(r#"{"not":"an array"}"#).is_err());
    }
}
