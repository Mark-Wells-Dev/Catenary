// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! A configurable mock standalone linter for testing the linter feeder.
//!
//! The linter analogue of `tools/mockls.rs`. A real linter is invoked as
//! `command <args…> <file…>` and writes findings to stdout in its own format;
//! `mocklint` emits canned findings in the shape of whichever adapter the linter
//! feeder will parse, echoing back the exact file paths it is given. That lets an
//! integration test drive the **real** `LinterFeeder` subprocess path (spawn →
//! parse → route → render) and the cross-feeder merge end-to-end without
//! depending on any linter being installed.
//!
//! Output format is selected by `--format`, mirroring the adapter the feeder
//! dispatches by `[linter.rule.<name>]` key:
//! - `shellcheck` → `shellcheck -f json1` (`{"comments": [...]}`),
//! - `actionlint` → `actionlint -format '{{json .}}'` (a JSON array),
//! - `yamllint` → `yamllint -f parsable` (one finding per text line),
//! - `sarif` → a SARIF log (`runs[].results[]`, the generic adapter).
//!
//! Findings are supplied with `--diag code|line|col|message` (repeatable); each
//! is emitted once per input file. `--exit-code` proves the feeder ignores exit
//! status (real linters exit nonzero on findings), and `--raw` is the
//! malformed-output escape hatch for the parse-failure fail-soft path.

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde_json::{Value, json};

/// Mock standalone linter for integration testing.
#[derive(Parser, Debug)]
#[command(name = "mocklint")]
struct Args {
    /// Output shape — selects which adapter format the findings are emitted in.
    #[arg(long, default_value = "shellcheck")]
    format: Format,

    /// A finding emitted for each input file: `code|line|col|message`
    /// (1-based line/col). Repeatable. `code` is the rule id (e.g. `SC2086`),
    /// `line`/`col` default to 1 and `message` to empty when omitted.
    #[arg(long = "diag")]
    diags: Vec<String>,

    /// Severity stamped on each finding, mapped into the format's level field
    /// (`actionlint` carries no level and ignores it).
    #[arg(long, default_value = "warning")]
    severity: String,

    /// Source name — the SARIF `tool.driver.name`. The blessed formats stamp a
    /// fixed source (their own name); only SARIF takes the source from output.
    #[arg(long, default_value = "mocklint")]
    source: String,

    /// Process exit code (default 0). Real linters exit nonzero when they find
    /// issues and the feeder ignores exit status; a test sets this nonzero to
    /// prove that.
    #[arg(long, default_value_t = 0)]
    exit_code: i32,

    /// Emit this text verbatim instead of formatted findings — an escape hatch
    /// for malformed-output / parse-failure tests.
    #[arg(long)]
    raw: Option<String>,

    /// Files to lint; each path is echoed back in the format's file field so the
    /// feeder's `resolve_reported_file` maps it onto the batch inputs.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,
}

/// The adapter output shape to emit.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Format {
    /// `shellcheck -f json1`: `{"comments": [...]}` with an integer `code`.
    Shellcheck,
    /// `actionlint -format '{{json .}}'`: a JSON array, `kind` as the code.
    Actionlint,
    /// `yamllint -f parsable`: `<file>:<line>:<col>: [<level>] <msg> (<rule>)`.
    Yamllint,
    /// A SARIF log (`runs[].results[]`), parsed by the generic adapter.
    Sarif,
}

/// One canned finding, parsed from a `--diag code|line|col|message` spec.
struct Finding {
    /// Rule id / code (e.g. `SC2086`, `syntax-check`, `DL3008`).
    code: String,
    /// 1-based line number.
    line: u64,
    /// 1-based column number.
    col: u64,
    /// Diagnostic message text.
    message: String,
}

/// Parses a `code|line|col|message` spec; missing fields take their defaults.
fn parse_diag(spec: &str) -> Finding {
    let mut parts = spec.splitn(4, '|');
    let code = parts.next().unwrap_or_default().to_string();
    let line = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let col = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let message = parts.next().unwrap_or_default().to_string();
    Finding {
        code,
        line,
        col,
        message,
    }
}

/// Renders the findings for every input file in the requested format.
fn render(args: &Args, findings: &[Finding]) -> String {
    match args.format {
        Format::Shellcheck => render_shellcheck(&args.files, findings, &args.severity),
        Format::Actionlint => render_actionlint(&args.files, findings),
        Format::Yamllint => render_yamllint(&args.files, findings, &args.severity),
        Format::Sarif => render_sarif(&args.files, findings, &args.severity, &args.source),
    }
}

/// Emits `shellcheck -f json1` output. `code` is rendered as an integer (the
/// adapter prepends `SC`); a non-numeric code is omitted so the adapter falls
/// back to the source name.
fn render_shellcheck(files: &[PathBuf], findings: &[Finding], severity: &str) -> String {
    let mut comments = Vec::new();
    for file in files {
        let path = file.to_string_lossy();
        for f in findings {
            let mut comment = json!({
                "file": path.as_ref(),
                "line": f.line,
                "endLine": f.line,
                "column": f.col,
                "endColumn": f.col,
                "level": severity,
                "message": f.message,
            });
            if let Some(n) = shellcheck_code(&f.code) {
                comment["code"] = json!(n);
            }
            comments.push(comment);
        }
    }
    json!({ "comments": comments }).to_string()
}

/// Parses a shellcheck integer code from a `SC2086`-style or bare-numeric id.
fn shellcheck_code(code: &str) -> Option<u64> {
    let digits = code
        .strip_prefix("SC")
        .or_else(|| code.strip_prefix("sc"))
        .unwrap_or(code);
    digits.parse().ok()
}

/// Emits `actionlint -format '{{json .}}'` output: a JSON array with `kind` as
/// the code. actionlint carries no level, so severity is not stamped.
fn render_actionlint(files: &[PathBuf], findings: &[Finding]) -> String {
    let mut items = Vec::new();
    for file in files {
        let path = file.to_string_lossy();
        for f in findings {
            items.push(json!({
                "message": f.message,
                "filepath": path.as_ref(),
                "line": f.line,
                "column": f.col,
                "kind": f.code,
            }));
        }
    }
    Value::Array(items).to_string()
}

/// Emits `yamllint -f parsable` text, one finding per line with the code in the
/// trailing `(rule)`.
fn render_yamllint(files: &[PathBuf], findings: &[Finding], severity: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for file in files {
        let path = file.to_string_lossy();
        for f in findings {
            let _ = writeln!(
                out,
                "{}:{}:{}: [{severity}] {} ({})",
                path.as_ref(),
                f.line,
                f.col,
                f.message,
                f.code,
            );
        }
    }
    out
}

/// Emits a SARIF log with one run whose `tool.driver.name` is `source`.
fn render_sarif(files: &[PathBuf], findings: &[Finding], severity: &str, source: &str) -> String {
    let mut results = Vec::new();
    for file in files {
        let path = file.to_string_lossy();
        for f in findings {
            results.push(json!({
                "ruleId": f.code,
                "level": severity,
                "message": { "text": f.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": path.as_ref() },
                        "region": { "startLine": f.line, "startColumn": f.col },
                    }
                }],
            }));
        }
    }
    json!({
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": { "name": source } },
            "results": results,
        }],
    })
    .to_string()
}

fn main() {
    let args = Args::parse();
    let output = args.raw.clone().unwrap_or_else(|| {
        let findings: Vec<Finding> = args.diags.iter().map(|s| parse_diag(s)).collect();
        render(&args, &findings)
    });

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{output}");
    let _ = lock.flush();

    std::process::exit(args.exit_code);
}
