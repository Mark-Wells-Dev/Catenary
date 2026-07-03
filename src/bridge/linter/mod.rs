// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Linter feeder — the second diagnostic feeder behind the [`DiagnosticFeeder`]
//! port (workstream 34 ticket 01).
//!
//! The canonical internal diagnostic shape is **LSP-diagnostic JSON**
//! (`source` / `code` / `range` / `severity` / `message`) — what the diagnostics
//! aggregator already consumes from language servers. A linter feeder's whole
//! job is to translate a standalone linter's output into that shape so the
//! downstream merge/format pass runs feeder-blind. The LSP client is the
//! protocol-native feeder and is left as-is; this module integrates the linter
//! feeders at the diagnostics batch.
//!
//! Adapters:
//! - **Blessed** (hand-rolled, keyed by linter name): [`shellcheck`],
//!   [`actionlint`], [`yamllint`]. Each guarantees `source` + `code` populated.
//! - **Generic SARIF** ([`sarif`]): one adapter for any SARIF-emitting tool a
//!   user wraps. No errorformat engine.
//!
//! Operational invariants every adapter owns:
//! - **Exit code is not failure.** Linters exit nonzero when they find issues;
//!   parsing keys on output, never on exit status.
//! - **Fail-soft.** A linter that is not installed is skipped with one notify; a
//!   parse failure drops that linter's diagnostics with a `warn!`. Neither ever
//!   crashes or poisons the diagnostics batch.

mod actionlint;
mod sarif;
mod shellcheck;
mod yamllint;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::bridge::filesystem_manager::FilesystemManager;
use crate::config::LinterConfig;
use crate::lsp::LspClientManager;

/// LSP-shaped diagnostics produced by a feeder for a single file.
///
/// `diagnostics` are LSP-diagnostic JSON objects (`source` / `code` / `range` /
/// `severity` / `message`); `command` is the producing linter's command, used
/// to pick the (passthrough) message filter at translation time.
///
/// A **present result with empty `diagnostics`** means the linter ran and found
/// nothing — a verification, not an absence (bug 56 ruling 2 / ticket 06): the
/// downstream feeder records it so the file classifies Clean. A linter that
/// never completed (not installed, spawn or parse failure) emits no result for
/// the file at all, leaving it unverified.
pub struct FeederDiagnostics {
    /// Absolute path of the file these diagnostics belong to (matched back onto
    /// the batch's canonical inputs).
    pub file: PathBuf,
    /// The linter command that produced them.
    pub command: String,
    /// LSP-shaped diagnostics.
    pub diagnostics: Vec<Value>,
}

/// One parsed diagnostic from a linter adapter, before file-path resolution.
///
/// `file` is the path string exactly as the linter reported it; the runner maps
/// it back onto the batch's canonical absolute paths.
pub struct RawLinterDiag {
    /// The file path as reported by the linter.
    pub file: String,
    /// The LSP-shaped diagnostic JSON object.
    pub diagnostic: Value,
}

/// Port: a feeder that publishes LSP-shaped diagnostics for a set of files.
///
/// The LSP client is the protocol-native feeder (not refactored onto this
/// trait, to avoid a risky rewrite of the diagnostics path); [`LinterFeeder`] is
/// the subprocess-and-parse adapter that runs standalone linters.
#[allow(
    async_fn_in_trait,
    reason = "single in-process adapter; the future is awaited in place in the diagnostics batch, never spawned across threads"
)]
pub trait DiagnosticFeeder {
    /// Produces LSP-shaped diagnostics for `files`.
    ///
    /// Fail-soft: an absent linter is skipped, a parse failure drops that
    /// linter's diagnostics, and the returned set carries whatever succeeded.
    async fn feed(&self, files: &[PathBuf]) -> Vec<FeederDiagnostics>;
}

/// The standalone-linter adapter behind the [`DiagnosticFeeder`] port.
///
/// Borrows the shared [`LspClientManager`] (effective linter set + per-root
/// `disable_lint`) and [`FilesystemManager`] (root resolution) for the duration
/// of one diagnostics batch.
pub struct LinterFeeder<'a> {
    manager: &'a LspClientManager,
    fs: &'a FilesystemManager,
}

impl<'a> LinterFeeder<'a> {
    /// Builds a feeder over the shared managers for one diagnostics batch.
    pub const fn new(manager: &'a LspClientManager, fs: &'a FilesystemManager) -> Self {
        Self { manager, fs }
    }
}

impl DiagnosticFeeder for LinterFeeder<'_> {
    async fn feed(&self, files: &[PathBuf]) -> Vec<FeederDiagnostics> {
        // Group the batch by owning root: routing globs and the effective linter
        // set are both per-root.
        let mut by_root: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for file in files {
            if let Some(root) = self.fs.resolve_root(file) {
                by_root.entry(root).or_default().push(file.clone());
            }
        }

        let mut out = Vec::new();
        for (root, root_files) in by_root {
            if self.manager.is_lint_disabled(&root) {
                continue;
            }
            let linters = self.manager.effective_linters(&root);
            // Deterministic linter order for stable output.
            let mut names: Vec<&String> = linters.keys().collect();
            names.sort();
            for name in names {
                let Some(linter) = linters.get(name) else {
                    continue;
                };
                if linter.disable || linter.command.is_empty() {
                    continue;
                }
                let matching: Vec<PathBuf> = root_files
                    .iter()
                    .filter(|f| {
                        f.strip_prefix(&root)
                            .is_ok_and(|rel| self.fs.linter_routes(linter, f, rel))
                    })
                    .cloned()
                    .collect();
                if matching.is_empty() {
                    continue;
                }
                out.extend(run_linter(name, linter, &root, &matching).await);
            }
        }
        out
    }
}

/// Runs one linter over its matching files and translates the output.
///
/// Fail-soft throughout: a not-installed linter, a spawn error, or a parse
/// failure each yields an empty result (after one `warn!`) rather than
/// propagating. Exit status is **not** consulted — linters exit nonzero when
/// they find issues.
///
/// A completed run (spawn + parse both succeeded) emits one [`FeederDiagnostics`]
/// per file it was handed — carrying that file's diagnostics, or an empty vec for
/// a file it found nothing wrong with. An empty result is a verification, not an
/// absence (bug 56 ruling 2 / ticket 06); the fail-soft early returns above emit
/// nothing, so a linter that never completed leaves its files unverified rather
/// than falsely clean.
async fn run_linter(
    name: &str,
    linter: &LinterConfig,
    root: &Path,
    files: &[PathBuf],
) -> Vec<FeederDiagnostics> {
    let mut cmd = tokio::process::Command::new(&linter.command);
    cmd.args(&linter.args);
    for file in files {
        cmd.arg(file);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Not installed → ONE notify, skip (not a hard error).
            warn!(
                linter = name,
                command = %linter.command,
                "linter '{name}' not found ({}); skipping — install it or set \
                 [linter.rule.{name}] disable = true",
                linter.command,
            );
            return Vec::new();
        }
        Err(e) => {
            warn!(
                linter = name,
                "linter '{name}' failed to run: {e}; skipping"
            );
            return Vec::new();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = match parse_output(name, &stdout) {
        Ok(parsed) => parsed,
        Err(e) => {
            // Parse failure → drop this linter's diagnostics + warn, never poison
            // the batch.
            warn!(
                linter = name,
                "linter '{name}' output parse failed: {e}; dropping its diagnostics",
            );
            return Vec::new();
        }
    };

    // Map each reported file back onto the batch's canonical inputs.
    let mut by_file: BTreeMap<PathBuf, Vec<Value>> = BTreeMap::new();
    for raw in parsed {
        if let Some(path) = resolve_reported_file(&raw.file, root, files) {
            by_file.entry(path).or_default().push(raw.diagnostic);
        } else {
            debug!(
                linter = name,
                file = %raw.file,
                "linter reported a file outside the batch; dropping its diagnostic",
            );
        }
    }

    // Emit a per-file result for every file the linter ran against — with its
    // diagnostics, or an empty vec for a file it found nothing wrong with. The
    // empty results are the verifications (bug 56 ruling 2 / ticket 06): the
    // feeder records them so the file classifies Clean rather than dropping to
    // NoResults, mirroring `retrieve_diagnostics`' record-even-with-zero rule.
    // Only reached once spawn + parse both succeed, so a linter that never
    // completed emits nothing and leaves its files unverified.
    files
        .iter()
        .map(|file| FeederDiagnostics {
            file: file.clone(),
            command: linter.command.clone(),
            diagnostics: by_file.remove(file).unwrap_or_default(),
        })
        .collect()
}

/// Dispatches parsing to the blessed adapter keyed by linter name, else SARIF.
///
/// An empty (clean) output short-circuits to no diagnostics so a JSON adapter
/// never errors on an empty document.
///
/// # Errors
///
/// Returns an error when the adapter cannot parse the output (malformed JSON,
/// missing required structure); the caller drops + warns.
fn parse_output(name: &str, output: &str) -> Result<Vec<RawLinterDiag>> {
    if output.trim().is_empty() {
        return Ok(Vec::new());
    }
    match name {
        "shellcheck" => shellcheck::parse(output),
        "actionlint" => actionlint::parse(output),
        "yamllint" => yamllint::parse(output),
        _ => sarif::parse(output),
    }
}

/// Resolves a linter-reported file path back onto a batch input path.
///
/// Strips a `file://` URI scheme (SARIF), resolves a relative path against
/// `root`, then matches against `candidates` by exact path, path suffix, or — as
/// a last resort — an unambiguous file-name match. Returns `None` when no input
/// path corresponds (the diagnostic is then dropped).
fn resolve_reported_file(reported: &str, root: &Path, candidates: &[PathBuf]) -> Option<PathBuf> {
    let stripped = reported.strip_prefix("file://").unwrap_or(reported);
    let reported_path = Path::new(stripped);
    let absolute: PathBuf = if reported_path.is_absolute() {
        reported_path.to_path_buf()
    } else {
        root.join(reported_path)
    };

    if let Some(hit) = candidates.iter().find(|c| **c == absolute) {
        return Some(hit.clone());
    }
    if let Some(hit) = candidates.iter().find(|c| c.ends_with(reported_path)) {
        return Some(hit.clone());
    }
    if let Some(name) = reported_path.file_name() {
        let mut named = candidates.iter().filter(|c| c.file_name() == Some(name));
        if let Some(first) = named.next()
            && named.next().is_none()
        {
            return Some(first.clone());
        }
    }
    None
}

/// Builds an LSP `range` JSON object from 1-based linter coordinates.
///
/// Linters report 1-based line/column; LSP ranges are 0-based, so each
/// coordinate is decremented (saturating, so a `0` or absent coordinate maps to
/// `0`). The diagnostics formatter adds 1 back for display.
pub(super) fn lsp_range(start_line: u64, start_col: u64, end_line: u64, end_col: u64) -> Value {
    json!({
        "start": { "line": to_zero_based(start_line), "character": to_zero_based(start_col) },
        "end": { "line": to_zero_based(end_line), "character": to_zero_based(end_col) },
    })
}

/// Converts a 1-based coordinate to 0-based, saturating at 0.
const fn to_zero_based(n: u64) -> u64 {
    n.saturating_sub(1)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_empty_is_clean() {
        let parsed = parse_output("shellcheck", "   \n ").expect("empty is clean");
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_output_malformed_json_errors() {
        // A malformed blessed-adapter document surfaces an error so the runner
        // drops + warns (fail-soft) rather than crashing the batch.
        assert!(parse_output("shellcheck", "{not json").is_err());
        assert!(parse_output("actionlint", "{not json").is_err());
        // Unknown linter name falls to the SARIF adapter.
        assert!(parse_output("my-custom-tool", "{not json").is_err());
    }

    #[test]
    fn resolve_reported_file_exact_and_suffix_and_filename() {
        let root = Path::new("/proj");
        let candidates = vec![
            PathBuf::from("/proj/scripts/build.sh"),
            PathBuf::from("/proj/deploy.sh"),
        ];
        // Exact absolute match.
        assert_eq!(
            resolve_reported_file("/proj/deploy.sh", root, &candidates),
            Some(PathBuf::from("/proj/deploy.sh")),
        );
        // file:// URI is stripped.
        assert_eq!(
            resolve_reported_file("file:///proj/deploy.sh", root, &candidates),
            Some(PathBuf::from("/proj/deploy.sh")),
        );
        // Relative (SARIF) path resolves against the root.
        assert_eq!(
            resolve_reported_file("scripts/build.sh", root, &candidates),
            Some(PathBuf::from("/proj/scripts/build.sh")),
        );
        // A file outside the batch yields None.
        assert_eq!(
            resolve_reported_file("/elsewhere/x.sh", root, &candidates),
            None
        );
    }

    #[test]
    fn lsp_range_is_zero_based() {
        let range = lsp_range(3, 7, 3, 12);
        assert_eq!(range["start"]["line"], 2);
        assert_eq!(range["start"]["character"], 6);
        assert_eq!(range["end"]["line"], 2);
        assert_eq!(range["end"]["character"], 11);
        // A 0/absent coordinate saturates to 0 rather than underflowing.
        assert_eq!(lsp_range(0, 0, 0, 0)["start"]["line"], 0);
    }

    #[tokio::test]
    async fn feed_skips_uninstalled_linter() {
        // A bogus command exercises the not-installed fail-soft path without any
        // real linter binary: spawn fails with NotFound, run_linter returns
        // empty, and the batch survives.
        let linter = LinterConfig::new(
            "catenary-nonexistent-linter-xyz",
            vec![],
            vec!["**/*.sh".to_string()],
        )
        .expect("compile");
        let root = Path::new("/proj");
        let files = vec![PathBuf::from("/proj/x.sh")];
        let out = run_linter("catenary-nonexistent-linter-xyz", &linter, root, &files).await;
        assert!(out.is_empty(), "uninstalled linter is skipped, not fatal");
        // A linter that never ran emits nothing, so the downstream feeder records
        // no result and the file stays unverified — never falsely `[clean]`
        // (bug 56 ruling 2 / ticket 06). Contrast the clean-run case below.
    }

    #[tokio::test]
    async fn run_linter_clean_run_records_empty_result_per_file() {
        // A linter that runs to completion and reports nothing (`true` exits 0 with
        // empty stdout) is a verification: run_linter emits one result per file it
        // was handed, each carrying empty diagnostics, so the feeder records the
        // file Clean instead of dropping it (bug 56 ruling 2 / ticket 06). This is
        // the ran-and-found-nothing half of the distinction that
        // feed_skips_uninstalled_linter covers for never-ran.
        let linter =
            LinterConfig::new("true", vec![], vec!["**/*.sh".to_string()]).expect("compile");
        let root = Path::new("/proj");
        let files = vec![PathBuf::from("/proj/a.sh"), PathBuf::from("/proj/b.sh")];
        let out = run_linter("shellcheck", &linter, root, &files).await;
        assert_eq!(
            out.len(),
            2,
            "a completed clean run records one result per file"
        );
        assert!(
            out.iter().all(|f| f.diagnostics.is_empty()),
            "a ran-and-found-nothing result carries empty diagnostics",
        );
        assert!(
            out.iter()
                .any(|f| f.file.as_path() == Path::new("/proj/a.sh")),
            "the first ran-against file is recorded",
        );
        assert!(
            out.iter()
                .any(|f| f.file.as_path() == Path::new("/proj/b.sh")),
            "the second ran-against file is recorded",
        );
    }
}
