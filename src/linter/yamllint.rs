// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! yamllint adapter (`yamllint -f parsable`).
//!
//! yamllint has no JSON formatter, so this parses its `parsable` text format,
//! one finding per line:
//!
//! ```text
//! <file>:<line>:<col>: [<level>] <message> (<rule>)
//! ```
//!
//! The trailing `(<rule>)` is the rule id, used as the `code`. `source` is
//! always `"yamllint"`. Lines that do not match the shape are skipped.

use anyhow::Result;
use serde_json::json;

use super::{RawLinterDiag, lsp_range};

/// The `source` field stamped on every yamllint diagnostic.
const SOURCE: &str = "yamllint";

/// Parses `yamllint -f parsable` output into LSP-shaped diagnostics.
///
/// # Errors
///
/// Never returns an error — the line parser skips anything it cannot read, so a
/// malformed line is dropped rather than failing the batch. The `Result` keeps
/// the adapter signature uniform with the JSON adapters.
#[allow(
    clippy::unnecessary_wraps,
    reason = "uniform adapter signature with the JSON adapters dispatched in parse_output"
)]
pub(super) fn parse(output: &str) -> Result<Vec<RawLinterDiag>> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some(diag) = parse_line(line) {
            out.push(diag);
        }
    }
    Ok(out)
}

/// Parses a single `parsable`-format line, or `None` if it does not match.
fn parse_line(line: &str) -> Option<RawLinterDiag> {
    // The "[level]" token starts the message portion; everything before the
    // preceding ": " is "path:line:col". The first ": [" is the level bracket
    // because the path:line:col prefix always precedes any message text.
    let bracket = line.find(": [")?;
    let (prefix, rest) = line.split_at(bracket);
    let rest = rest.strip_prefix(": ")?; // "[level] message (rule)"

    // prefix = path:line:col — peel col then line off the right.
    let (path_and_line, col_str) = prefix.rsplit_once(':')?;
    let (path, line_str) = path_and_line.rsplit_once(':')?;
    let line_no: u64 = line_str.trim().parse().ok()?;
    let col_no: u64 = col_str.trim().parse().ok()?;

    // rest = "[level] message (rule)"
    let after_open = rest.strip_prefix('[')?;
    let (level, message_and_rule) = after_open.split_once(']')?;
    let message_and_rule = message_and_rule.trim_start();
    let (message, code) = extract_rule(message_and_rule);

    Some(RawLinterDiag {
        file: path.to_string(),
        diagnostic: json!({
            // yamllint reports a point, not a span — end == start.
            "range": lsp_range(line_no, col_no, line_no, col_no),
            "severity": level_to_severity(level),
            "source": SOURCE,
            "code": code,
            "message": message,
        }),
    })
}

/// Splits a trailing `(rule)` off the message, returning `(message, code)`.
///
/// A bare token in trailing parens (no spaces) is the rule id; otherwise the
/// whole string is the message and the code falls back to `"yamllint"` so it is
/// always populated.
fn extract_rule(text: &str) -> (String, String) {
    if let Some(stripped) = text.strip_suffix(')')
        && let Some(open) = stripped.rfind('(')
    {
        let rule = &stripped[open + 1..];
        if !rule.is_empty() && !rule.contains(' ') {
            return (stripped[..open].trim_end().to_string(), rule.to_string());
        }
    }
    (text.to_string(), SOURCE.to_string())
}

/// Maps a yamllint `level` to an LSP severity (1=Error, 2=Warning).
fn level_to_severity(level: &str) -> u8 {
    match level.trim() {
        "error" => 1,
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

    /// Golden `yamllint -f parsable` output.
    const GOLDEN: &str = "\
/proj/config.yaml:1:1: [warning] missing document start \"---\" (document-start)
/proj/config.yaml:3:19: [error] trailing spaces (trailing-spaces)
/proj/config.yaml:7:1: [error] syntax error: could not find expected ':' (syntax)
";

    #[test]
    fn parses_golden_to_lsp_diagnostics() {
        let diags = parse(GOLDEN).expect("parse golden");
        assert_eq!(diags.len(), 3);

        let first = &diags[0];
        assert_eq!(first.file, "/proj/config.yaml");
        let d = &first.diagnostic;
        assert_eq!(d["source"], "yamllint");
        assert_eq!(d["code"], "document-start");
        assert_eq!(d["severity"], 2); // warning
        // 1-based (1,1) → 0-based (0,0).
        assert_eq!(d["range"]["start"]["line"], 0);
        assert_eq!(d["range"]["start"]["character"], 0);
        assert_eq!(d["message"], "missing document start \"---\"");

        let second = &diags[1];
        assert_eq!(second.diagnostic["code"], "trailing-spaces");
        assert_eq!(second.diagnostic["severity"], 1); // error
        assert_eq!(second.diagnostic["range"]["start"]["line"], 2);
        assert_eq!(second.diagnostic["range"]["start"]["character"], 18);

        let third = &diags[2];
        assert_eq!(third.diagnostic["code"], "syntax");
        assert_eq!(
            third.diagnostic["message"],
            "syntax error: could not find expected ':'"
        );
    }

    #[test]
    fn clean_output_yields_no_diagnostics() {
        let diags = parse("").expect("parse clean");
        assert!(diags.is_empty());
    }

    #[test]
    fn line_without_rule_parens_falls_back_to_source_code() {
        let diags = parse("/a.yaml:2:3: [warning] some freeform message").expect("parse");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].diagnostic["code"], "yamllint");
        assert_eq!(diags[0].diagnostic["message"], "some freeform message");
    }

    #[test]
    fn unparseable_lines_are_skipped() {
        let diags = parse("this is not a yamllint line\n").expect("parse");
        assert!(diags.is_empty());
    }

    #[test]
    fn message_with_parenthetical_keeps_rule_only_when_bare() {
        // A parenthetical containing spaces is part of the message, not a rule.
        let diags = parse("/a.yaml:1:1: [error] bad value (expected one thing)").expect("parse");
        assert_eq!(
            diags[0].diagnostic["message"],
            "bad value (expected one thing)"
        );
        assert_eq!(diags[0].diagnostic["code"], "yamllint");
    }
}
