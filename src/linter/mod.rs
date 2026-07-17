// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The shared standalone-linter core (ws43-04).
//!
//! Spawn discipline, output parsing, severity mapping, and routing —
//! stateless, pool-less, and callable with **no daemon at all**.
//!
//! Two surfaces consume this core, and neither duplicates its logic:
//!
//! - The **daemon diagnostics pipeline** (`catenary diagnostics`), through the
//!   [`LinterFeeder`](crate::bridge::linter::LinterFeeder) adapter in
//!   `src/bridge/linter.rs` — the second diagnostic feeder behind the
//!   `DiagnosticFeeder` port (workstream 34 ticket 01).
//! - The **CLI query sink** (`catenary grep`), through
//!   [`LintAnnotator`](crate::hitstream::lint::LintAnnotator) — lint-covered
//!   hit batches stream to locally-spawned linters instead of the daemon
//!   (ws43-04), so pool-less lint work never requires a daemon.
//!
//! The canonical internal diagnostic shape is **LSP-diagnostic JSON**
//! (`source` / `code` / `range` / `severity` / `message`) — what the diagnostics
//! aggregator already consumes from language servers. A linter adapter's whole
//! job is to translate a standalone linter's output into that shape so every
//! downstream pass runs feeder-blind.
//!
//! ## Routing — the source of truth
//!
//! Which linter covers which file is **config-derived, per root**:
//!
//! 1. The effective linter set for a root is the user `[linter.rule.*]` unioned
//!    with the root's project `.catenary.toml` `[linter.rule.*]`, the project
//!    winning on a name collision ([`merge_effective_linters`] — the single
//!    merge both the daemon's
//!    [`LspClientManager::effective_linters`](crate::lsp::LspClientManager::effective_linters)
//!    and the CLI-side [`LintRouter`] call).
//! 2. A root's `[linter] disable = true` drops the whole set for that root.
//! 3. A file routes to an enabled linter when its root-relative path matches a
//!    routing glob **or** its `#!` interpreter basename is declared
//!    ([`FilesystemManager::linter_routes`](crate::bridge::filesystem_manager::FilesystemManager::linter_routes)
//!    — the one routing predicate, shared by the editing gate, the diagnostics
//!    fan-out, and the query sink).
//!
//! Daemon-side, the owning root comes from the registered-roots ledger;
//! CLI-side (no ledger without a daemon), [`LintRouter`] resolves it as the
//! enclosing worktree root
//! ([`companions::enclosing_worktree_root`](crate::companions::enclosing_worktree_root))
//! — the same discovery the daemon's query auto-mount uses, so both sides
//! answer the same root for any file inside a repository.
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
//! - **Fail-soft.** A linter that is not installed, fails to spawn, or emits
//!   unparseable output yields a typed [`LinterRunOutcome`] the caller degrades
//!   on — a warn plus a dropped feed daemon-side, a stderr advisory plus
//!   pass-through hits CLI-side. Never a crash, never a poisoned batch, never a
//!   dropped hit.
//! - **Argv, never shell.** The linter command and its files are spawned as an
//!   argv vector; no shell ever interprets a path.

mod actionlint;
mod router;
mod sarif;
mod shellcheck;
mod yamllint;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use tracing::debug;

use crate::config::LinterConfig;

pub use router::{LintJob, LintRouter};

/// LSP-shaped diagnostics produced by a linter run for a single file.
///
/// `diagnostics` are LSP-diagnostic JSON objects (`source` / `code` / `range` /
/// `severity` / `message`); `command` is the producing linter's command, used
/// to pick the (passthrough) message filter at translation time.
///
/// A **present result with empty `diagnostics`** means the linter ran and found
/// nothing — a verification, not an absence (bug 56 ruling 2 / ticket 06): the
/// downstream consumer records it so the file classifies Clean. A linter that
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

/// What one linter run did — the typed fail-soft seam.
///
/// The core never logs or prints: each consumer maps the outcome onto its own
/// degrade surface (the daemon feeder warns into the firehose, the CLI query
/// sink advises on stderr), so the spawn/parse discipline lives here once and
/// the reporting discipline lives with each surface.
pub enum LinterRunOutcome {
    /// Spawn and parse both succeeded. One [`FeederDiagnostics`] per file the
    /// linter was handed — its diagnostics, or an empty vec for a file it found
    /// nothing wrong with (a verification, never an absence).
    Completed(Vec<FeederDiagnostics>),
    /// The linter's command was not found — not installed. The caller skips it
    /// with one notify; never a hard error.
    NotInstalled,
    /// The linter failed to spawn (permissions, exec format, …).
    SpawnFailed(String),
    /// The linter ran but its output did not parse; its diagnostics are
    /// dropped rather than poisoning the batch.
    ParseFailed(String),
}

/// The effective linter set for one root.
///
/// The user `[linter.rule.*]` unioned with the root's project
/// `[linter.rule.*]`, the project winning on a name collision (so a project
/// entry can override or `disable` a user-configured linter).
///
/// The single merge rule — the daemon's
/// [`LspClientManager::effective_linters`](crate::lsp::LspClientManager::effective_linters)
/// and the CLI-side [`LintRouter`] both delegate here, so query-time routing
/// and diagnostics-time routing cannot drift.
#[must_use]
#[allow(
    clippy::implicit_hasher,
    reason = "both layers are config-owned std HashMaps; generalizing the hasher buys nothing"
)]
pub fn merge_effective_linters(
    user: &HashMap<String, LinterConfig>,
    project: &HashMap<String, LinterConfig>,
) -> HashMap<String, LinterConfig> {
    let mut linters = user.clone();
    for (name, linter) in project {
        linters.insert(name.clone(), linter.clone());
    }
    linters
}

/// Runs one linter over its matching files and translates the output.
///
/// The spawn discipline lives here, once, for every consumer: the command and
/// its files are an **argv vector** (never a shell), stdin is null, output is
/// captured, and `kill_on_drop` ties the subprocess lifetime to this future —
/// a caller that times the future out (the CLI's annotation budget) or drops it
/// (cancel-on-disconnect, bug 98) also stops the child.
///
/// Exit status is **not** consulted — linters exit nonzero when they find
/// issues. A [`LinterRunOutcome::Completed`] carries one [`FeederDiagnostics`]
/// per file the linter was handed — with that file's diagnostics, or an empty
/// vec for a file it found nothing wrong with (bug 56 ruling 2 / ticket 06: an
/// empty result is a verification, not an absence). The failure outcomes emit
/// nothing per-file, so a linter that never completed leaves its files
/// unverified rather than falsely clean.
pub async fn run_linter(
    name: &str,
    linter: &LinterConfig,
    root: &Path,
    files: &[PathBuf],
) -> LinterRunOutcome {
    let mut cmd = tokio::process::Command::new(&linter.command);
    cmd.args(&linter.args);
    for file in files {
        cmd.arg(file);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Tie the subprocess to this future: a dropped or timed-out future must
    // never leave a detached linter running with its output going nowhere
    // (bug 98; the CLI's per-batch budget relies on this too).
    cmd.kill_on_drop(true);

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LinterRunOutcome::NotInstalled;
        }
        Err(e) => {
            return LinterRunOutcome::SpawnFailed(e.to_string());
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = match parse_output(name, &stdout) {
        Ok(parsed) => parsed,
        Err(e) => {
            return LinterRunOutcome::ParseFailed(format!("{e:#}"));
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
    // consumer records them so the file classifies Clean rather than dropping
    // to NoResults, mirroring `retrieve_diagnostics`' record-even-with-zero
    // rule. Only reached once spawn + parse both succeed, so a linter that
    // never completed emits nothing and leaves its files unverified.
    LinterRunOutcome::Completed(
        files
            .iter()
            .map(|file| FeederDiagnostics {
                file: file.clone(),
                command: linter.command.clone(),
                diagnostics: by_file.remove(file).unwrap_or_default(),
            })
            .collect(),
    )
}

/// Dispatches parsing to the blessed adapter keyed by linter name, else SARIF.
///
/// An empty (clean) output short-circuits to no diagnostics so a JSON adapter
/// never errors on an empty document.
///
/// # Errors
///
/// Returns an error when the adapter cannot parse the output (malformed JSON,
/// missing required structure); the caller drops + degrades.
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
pub(crate) fn lsp_range(start_line: u64, start_col: u64, end_line: u64, end_col: u64) -> Value {
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
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
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
        // reports ParseFailed (fail-soft) rather than crashing the batch.
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

    #[test]
    fn merge_effective_linters_project_wins() {
        let user_entry =
            LinterConfig::new("shellcheck", vec![], vec!["**/*.sh".to_string()]).expect("compile");
        let mut project_entry = user_entry.clone();
        project_entry.disable = true;
        let user: HashMap<String, LinterConfig> =
            std::iter::once(("shellcheck".to_string(), user_entry)).collect();
        let project: HashMap<String, LinterConfig> =
            std::iter::once(("shellcheck".to_string(), project_entry)).collect();

        let merged = merge_effective_linters(&user, &project);
        assert!(
            merged.get("shellcheck").is_some_and(|l| l.disable),
            "a project entry overrides the user entry by name",
        );
        // With no project overlay the user entry stands.
        let merged = merge_effective_linters(&user, &HashMap::new());
        assert!(merged.get("shellcheck").is_some_and(|l| !l.disable));
    }

    #[tokio::test]
    async fn run_linter_not_installed_is_typed_not_fatal() {
        // A bogus command exercises the not-installed fail-soft path without any
        // real linter binary: spawn fails with NotFound and the outcome is the
        // typed NotInstalled — the caller degrades, the batch survives.
        let linter = LinterConfig::new(
            "catenary-nonexistent-linter-xyz",
            vec![],
            vec!["**/*.sh".to_string()],
        )
        .expect("compile");
        let root = Path::new("/proj");
        let files = vec![PathBuf::from("/proj/x.sh")];
        let out = run_linter("catenary-nonexistent-linter-xyz", &linter, root, &files).await;
        assert!(
            matches!(out, LinterRunOutcome::NotInstalled),
            "an uninstalled linter is a typed skip, not fatal"
        );
        // A linter that never ran emits nothing, so the consumer records no
        // result and the file stays unverified — never falsely `[clean]`
        // (bug 56 ruling 2 / ticket 06). Contrast the clean-run case below.
    }

    #[tokio::test]
    async fn run_linter_clean_run_records_empty_result_per_file() {
        // A linter that runs to completion and reports nothing (`true` exits 0 with
        // empty stdout) is a verification: run_linter emits one result per file it
        // was handed, each carrying empty diagnostics, so the consumer records the
        // file Clean instead of dropping it (bug 56 ruling 2 / ticket 06). This is
        // the ran-and-found-nothing half of the distinction that
        // run_linter_not_installed_is_typed_not_fatal covers for never-ran.
        let linter =
            LinterConfig::new("true", vec![], vec!["**/*.sh".to_string()]).expect("compile");
        let root = Path::new("/proj");
        let files = vec![PathBuf::from("/proj/a.sh"), PathBuf::from("/proj/b.sh")];
        let LinterRunOutcome::Completed(out) =
            run_linter("shellcheck", &linter, root, &files).await
        else {
            panic!("a clean run completes");
        };
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
