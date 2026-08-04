// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The local linter sink (ws43-04): the stateless annotator.
//!
//! The CLI streams hit-batches to TWO sink kinds. The daemon is the stateful
//! annotator — LSP enrichment needs the pool. Lint-covered files need no pool:
//! their annotator is a **locally-spawned linter**, so their hits route here
//! and never touch the daemon — which is exactly what makes lint annotation
//! part of the degrade story: a lint-covered file's hits come back annotated
//! with no daemon at all.
//!
//! Per batch, [`LintAnnotator`] partitions hits by the shared routing rules
//! ([`crate::linter`] — the routing source of truth), runs each covering
//! linter over the batch's not-yet-linted files through the shared core's
//! [`run_linter`] (spawn discipline, parsing, and severity mapping move
//! intact), and maps each lint-covered hit onto the wire [`AnnotatedHit`]
//! tri-state:
//!
//! - a diagnostic on the hit's line → the anchor trail `source/code`
//!   (`build.sh:3#shellcheck/SC2086:…`);
//! - file verified, nothing on this line → enriched with no anchor (the
//!   covered, clean spelling);
//! - no covering linter completed (not installed, spawn/parse failure, blown
//!   budget) → pass-through (`#?`), never a dropped hit.
//!
//! Budget discipline: every linter run is wrapped in the annotator's budget
//! (default [`ANNOTATION_BATCH_BUDGET`]) — `kill_on_drop` in the shared core
//! stops the timed-out subprocess — and a linter that blows its budget, fails,
//! or is missing is marked **dead for the run**: its later batches pass
//! through instantly, so a wedged linter costs one budget once, never a
//! stalled stream. Each degrade records ONE stderr-bound advisory, surfaced by
//! the caller after the stream (advisories never ride the result channel).
//!
//! Lint results are cached per file for the life of one annotator (one
//! query): a file spanning many batches is linted once.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::bridge::filesystem_manager::FilesystemManager;
use crate::config::LinterConfig;
use crate::linter::{LintRouter, LinterRunOutcome, run_linter};

use super::frame::AnnotatedHit;
use super::{ANNOTATION_BATCH_BUDGET, HitBatch, WireHit};

/// One walk batch after the lint stage: the original hits with the per-hit
/// route mask and the resolved lint annotations attached.
///
/// `lint_mask[i]` is `true` when `hits[i]` routed to the local linter sink;
/// `lint` then carries the annotations for exactly the masked hits, in hit
/// order — resolved before the batch travels further, so a daemon-side degrade
/// downstream can never lose them.
pub struct LintedBatch {
    /// Monotonic batch sequence number (the walk's).
    pub seq: u64,
    /// The batch's hits, in the walk's global order.
    pub hits: Vec<WireHit>,
    /// Per-hit route: `true` = lint-covered (annotated locally, never sent to
    /// the daemon), `false` = the daemon annotator's.
    pub lint_mask: Vec<bool>,
    /// Annotations for the masked hits, in hit order (`lint.len()` equals the
    /// mask's `true` count).
    pub lint: Vec<AnnotatedHit>,
}

/// One file's lint outcome for this run.
enum FileLint {
    /// At least one covering linter completed over this file; `lines` maps a
    /// 1-based hit line to its anchor trail (`source/code`), first diagnostic
    /// per line winning (linters run in deterministic name order).
    Verified { lines: HashMap<u32, String> },
    /// No covering linter completed — the file's hits pass through (`#?`).
    Unverified,
}

/// The local lint annotator: per-file routing, budgeted linter runs, per-file
/// result caching, and the run's advisory ledger.
pub struct LintAnnotator {
    router: LintRouter,
    /// Per-file outcome, cached for the run (a file spanning several batches
    /// is linted once).
    files: HashMap<PathBuf, FileLint>,
    /// Linters dead for this run (not installed, failed, or budget-blown):
    /// their files pass through instantly — a wedged linter costs one budget
    /// once.
    dead: HashSet<String>,
    /// One advisory per degrade cause, in occurrence order.
    advisories: Vec<String>,
    /// Per-linter-run budget.
    budget: Duration,
}

impl LintAnnotator {
    /// Builds an annotator over the user `[linter.rule.*]` layer and the
    /// shared filesystem classification cache, with the standard annotation
    /// budget.
    #[must_use]
    pub fn new(user_linters: HashMap<String, LinterConfig>, fs: Arc<FilesystemManager>) -> Self {
        Self::with_budget(user_linters, fs, ANNOTATION_BATCH_BUDGET)
    }

    /// [`Self::new`] with an explicit per-run budget (tests).
    #[must_use]
    pub fn with_budget(
        user_linters: HashMap<String, LinterConfig>,
        fs: Arc<FilesystemManager>,
        budget: Duration,
    ) -> Self {
        Self {
            router: LintRouter::new(user_linters, fs),
            files: HashMap::new(),
            dead: HashSet::new(),
            advisories: Vec::new(),
            budget,
        }
    }

    /// Whether `path` routes to the local linter sink (the per-hit mask
    /// predicate).
    pub fn covers(&mut self, path: &Path) -> bool {
        self.router.covers(path)
    }

    /// Annotates one batch's lint-covered hits, running any not-yet-linted
    /// files' covering linters under budget. Returns one [`AnnotatedHit`] per
    /// input hit, in order — pass-through for anything a linter never
    /// verified, never a dropped hit.
    pub async fn annotate(&mut self, hits: Vec<WireHit>) -> Vec<AnnotatedHit> {
        // The batch's distinct files that still need a lint pass, in first-hit
        // order (deterministic runs).
        let mut pending: Vec<PathBuf> = Vec::new();
        for hit in &hits {
            if !self.files.contains_key(&hit.path) && !pending.contains(&hit.path) {
                pending.push(hit.path.clone());
            }
        }

        if !pending.is_empty() {
            self.lint_files(&pending).await;
            // Anything no completed run verified stays pass-through for the
            // whole query — its covering linters are dead or its runs failed.
            for file in pending {
                self.files.entry(file).or_insert(FileLint::Unverified);
            }
        }

        hits.into_iter()
            .map(|hit| match self.files.get(&hit.path) {
                Some(FileLint::Verified { lines }) => lines.get(&hit.line).map_or_else(
                    || AnnotatedHit::top_level(hit.clone()),
                    |trail| AnnotatedHit::scoped(hit.clone(), trail.clone()),
                ),
                _ => AnnotatedHit::passthrough(hit),
            })
            .collect()
    }

    /// Consumes the run's advisory ledger (one line per degrade cause).
    pub fn take_advisories(&mut self) -> Vec<String> {
        std::mem::take(&mut self.advisories)
    }

    /// Runs the covering linters over `files` (skipping dead linters), folding
    /// each completed run's diagnostics into the per-file cache and each
    /// degrade into the dead set + advisory ledger.
    async fn lint_files(&mut self, files: &[PathBuf]) {
        let jobs = self.router.plan(files);
        for job in jobs {
            if self.dead.contains(&job.name) {
                continue;
            }
            let outcome = tokio::time::timeout(
                self.budget,
                run_linter(&job.name, &job.linter, &job.root, &job.files),
            )
            .await;
            match outcome {
                Ok(LinterRunOutcome::Completed(feeds)) => {
                    for feed in feeds {
                        // A completed run verifies its file (empty diagnostics
                        // are the ran-and-found-nothing verification); an
                        // earlier Unverified slot upgrades — any completed
                        // covering linter is coverage.
                        let slot = self.files.entry(feed.file).or_insert(FileLint::Unverified);
                        if matches!(slot, FileLint::Unverified) {
                            *slot = FileLint::Verified {
                                lines: HashMap::new(),
                            };
                        }
                        let FileLint::Verified { lines } = slot else {
                            continue;
                        };
                        for diag in &feed.diagnostics {
                            if let Some((line, trail)) = diag_trail(diag) {
                                lines.entry(line).or_insert(trail);
                            }
                        }
                    }
                }
                Ok(LinterRunOutcome::NotInstalled) => {
                    self.mark_dead(
                        &job.name,
                        format!(
                            "[lint: linter '{}' not found ({}) — hits pass through unannotated; \
                             install it or set [linter.rule.{}] disable = true]",
                            job.name, job.linter.command, job.name,
                        ),
                    );
                }
                Ok(LinterRunOutcome::SpawnFailed(e)) => {
                    self.mark_dead(
                        &job.name,
                        format!(
                            "[lint: linter '{}' failed to run: {e} — hits pass through unannotated]",
                            job.name,
                        ),
                    );
                }
                Ok(LinterRunOutcome::ParseFailed(e)) => {
                    self.mark_dead(
                        &job.name,
                        format!(
                            "[lint: linter '{}' output parse failed: {e} — hits pass through \
                             unannotated]",
                            job.name,
                        ),
                    );
                }
                Err(_elapsed) => {
                    // The budget bounds enrichment, never hits: the timed-out
                    // future drops (killing the subprocess via kill_on_drop) and
                    // the linter is dead for the rest of the run, so a wedged
                    // linter never stalls the stream twice.
                    self.mark_dead(
                        &job.name,
                        format!(
                            "[lint: linter '{}' exceeded its annotation budget — hits pass \
                             through unannotated]",
                            job.name,
                        ),
                    );
                }
            }
        }
    }

    /// Marks a linter dead for the run with its one advisory.
    fn mark_dead(&mut self, name: &str, advisory: String) {
        if self.dead.insert(name.to_string()) {
            self.advisories.push(advisory);
        }
    }
}

/// Extracts a hit-line anchor from one LSP-shaped diagnostic: the 1-based line
/// and the `source/code` trail (`code` omitted when absent).
fn diag_trail(diag: &Value) -> Option<(u32, String)> {
    let line0 = diag
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)?;
    let line = u32::try_from(line0).ok()?.checked_add(1)?;
    let source = diag.get("source").and_then(Value::as_str).unwrap_or("lint");
    let code = diag.get("code").and_then(|c| {
        c.as_str()
            .map(str::to_string)
            .or_else(|| c.as_u64().map(|n| n.to_string()))
    });
    let trail = code.map_or_else(|| source.to_string(), |c| format!("{source}/{c}"));
    Some((line, trail))
}

/// The lint stage between the walk and the wire.
///
/// Consumes the walk's ordered batches, partitions each by lint coverage,
/// resolves the lint-covered hits' annotations locally, and forwards each
/// [`LintedBatch`] downstream in the same order.
///
/// With no annotator (a broken user config), every batch forwards with an
/// all-`false` mask — the pre-ws43-04 behavior, everything to the daemon.
/// Returns the run's advisory lines for the caller's stderr.
pub async fn lint_stage(
    mut annotator: Option<LintAnnotator>,
    mut rx: tokio::sync::mpsc::Receiver<HitBatch>,
    tx: tokio::sync::mpsc::Sender<LintedBatch>,
) -> Vec<String> {
    while let Some(batch) = rx.recv().await {
        let lint_mask: Vec<bool> = match annotator.as_mut() {
            Some(annotator) => batch
                .hits
                .iter()
                .map(|hit| annotator.covers(&hit.path))
                .collect(),
            None => vec![false; batch.hits.len()],
        };
        let lint = if lint_mask.contains(&true) {
            let masked: Vec<WireHit> = batch
                .hits
                .iter()
                .zip(&lint_mask)
                .filter(|&(_, &is_lint)| is_lint)
                .map(|(hit, _)| hit.clone())
                .collect();
            match annotator.as_mut() {
                Some(annotator) => annotator.annotate(masked).await,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let linted = LintedBatch {
            seq: batch.seq,
            hits: batch.hits,
            lint_mask,
            lint,
        };
        if tx.send(linted).await.is_err() {
            // Downstream gone (sink error / teardown): stop consuming.
            break;
        }
    }
    annotator
        .as_mut()
        .map(LintAnnotator::take_advisories)
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// A tempdir with a `.git` marker so the router resolves it as a root.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("git marker");
        dir
    }

    /// Writes an executable stub linter script emitting `body` on stdout.
    #[cfg(unix)]
    fn stub_linter(dir: &Path, name: &str, body: &str) -> PathBuf {
        stub_script(
            dir,
            name,
            &format!("cat <<'CATENARY_EOF'\n{body}\nCATENARY_EOF"),
        )
    }

    /// Writes an executable `#!/bin/sh` script running `commands`.
    #[cfg(unix)]
    fn stub_script(dir: &Path, name: &str, commands: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{commands}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        path
    }

    fn layer(name: &str, command: &str, patterns: &[&str]) -> HashMap<String, LinterConfig> {
        let linter = LinterConfig::new(
            command,
            vec![],
            patterns.iter().map(|p| (*p).to_string()).collect(),
        )
        .expect("compile");
        std::iter::once((name.to_string(), linter)).collect()
    }

    fn hit(path: &Path, line: u32, text: &str) -> WireHit {
        WireHit {
            path: path.to_path_buf(),
            line,
            column: 1,
            text: text.to_string(),
        }
    }

    /// A verified-clean file's hits render enriched (covered, no anchor), and a
    /// diagnostic line carries the `source/code` trail.
    #[cfg(unix)]
    #[tokio::test]
    async fn lint_annotates_diag_lines_and_marks_clean_lines_covered() {
        let repo = repo();
        let root = repo.path().canonicalize().expect("canonical");
        let sh = root.join("build.sh");
        std::fs::write(&sh, "echo $HOME\necho done\n").expect("write");

        // A shellcheck-shaped stub reporting SC2086 on line 1.
        let json = format!(
            "{{\"comments\":[{{\"file\":\"{}\",\"line\":1,\"endLine\":1,\"column\":6,\
             \"endColumn\":6,\"level\":\"warning\",\"code\":2086,\"message\":\"quote it\"}}]}}",
            sh.display()
        );
        let stub = stub_linter(&root, "stub-shellcheck", &json);
        let mut annotator = LintAnnotator::new(
            layer("shellcheck", &stub.to_string_lossy(), &["**/*.sh"]),
            Arc::new(FilesystemManager::new()),
        );

        assert!(annotator.covers(&sh), "the .sh routes to the stub linter");
        let annotated = annotator
            .annotate(vec![hit(&sh, 1, "echo $HOME"), hit(&sh, 2, "echo done")])
            .await;
        assert_eq!(annotated.len(), 2, "every hit survives annotation");
        assert_eq!(
            annotated[0].anchor.as_deref(),
            Some("shellcheck/SC2086"),
            "a diagnostic on the hit's line becomes the source/code trail",
        );
        assert!(annotated[0].enriched);
        assert!(
            annotated[1].enriched && annotated[1].anchor.is_none(),
            "a verified clean line renders covered, not `#?`",
        );
        assert!(
            annotator.take_advisories().is_empty(),
            "a healthy run records no advisory",
        );
    }

    /// An absent linter degrades every covered hit to pass-through with ONE
    /// advisory — never an error, never a dropped hit, and later batches skip
    /// the dead linter without re-spawning.
    #[tokio::test]
    async fn absent_linter_passes_through_with_one_advisory() {
        let repo = repo();
        let root = repo.path().canonicalize().expect("canonical");
        let sh = root.join("build.sh");
        std::fs::write(&sh, "echo hi\n").expect("write");

        let mut annotator = LintAnnotator::new(
            layer(
                "shellcheck",
                "catenary-definitely-not-a-linter",
                &["**/*.sh"],
            ),
            Arc::new(FilesystemManager::new()),
        );
        let first = annotator.annotate(vec![hit(&sh, 1, "echo hi")]).await;
        assert_eq!(first.len(), 1);
        assert!(
            !first[0].enriched && first[0].anchor.is_none(),
            "an unverified file's hit passes through",
        );

        // A second batch (another file, same dead linter) also passes through
        // and records no second advisory.
        let sh2 = root.join("deploy.sh");
        std::fs::write(&sh2, "echo two\n").expect("write");
        let second = annotator.annotate(vec![hit(&sh2, 1, "echo two")]).await;
        assert!(!second[0].enriched);

        let advisories = annotator.take_advisories();
        assert_eq!(advisories.len(), 1, "one advisory per absent linter");
        assert!(
            advisories[0].contains("not found"),
            "the advisory names the cause: {advisories:?}",
        );
    }

    /// A wedged linter (sleep stub) blows the budget once: its batch passes
    /// through, the linter is dead for the run, and the annotator answers —
    /// never a stalled stream.
    #[cfg(unix)]
    #[tokio::test]
    async fn wedged_linter_passes_through_within_budget() {
        let repo = repo();
        let root = repo.path().canonicalize().expect("canonical");
        let sh = root.join("build.sh");
        std::fs::write(&sh, "echo hi\n").expect("write");

        // A genuine wedge: the script ignores its file arguments and sleeps
        // (bare `sleep 60 <files>` would error out instantly instead).
        let wedge = stub_script(&root, "stub-wedge", "sleep 60");
        let mut layer = HashMap::new();
        let linter = LinterConfig::new(
            wedge.to_string_lossy().into_owned(),
            vec![],
            vec!["**/*.sh".to_string()],
        )
        .expect("compile");
        layer.insert("wedged".to_string(), linter);

        let mut annotator = LintAnnotator::with_budget(
            layer,
            Arc::new(FilesystemManager::new()),
            Duration::from_millis(100),
        );
        let start = std::time::Instant::now();
        let annotated = annotator.annotate(vec![hit(&sh, 1, "echo hi")]).await;
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "a wedged linter yields within the budget, not the sleep",
        );
        assert_eq!(annotated.len(), 1, "the hit survives the blown budget");
        assert!(!annotated[0].enriched, "a budget blow is a pass-through");

        // Dead for the run: a later batch answers instantly.
        let sh2 = root.join("deploy.sh");
        std::fs::write(&sh2, "echo two\n").expect("write");
        let start = std::time::Instant::now();
        let second = annotator.annotate(vec![hit(&sh2, 1, "echo two")]).await;
        assert!(
            start.elapsed() < Duration::from_millis(90),
            "a dead linter never respawns this run",
        );
        assert!(!second[0].enriched);

        let advisories = annotator.take_advisories();
        assert_eq!(advisories.len(), 1, "one advisory for the budget blow");
        assert!(advisories[0].contains("budget"), "{advisories:?}");
    }

    /// `diag_trail` maps an LSP-shaped diagnostic onto (1-based line,
    /// `source/code`), tolerating an absent code.
    #[test]
    fn diag_trail_extracts_line_and_source_code() {
        let diag = serde_json::json!({
            "source": "yamllint",
            "code": "line-length",
            "range": { "start": { "line": 4, "character": 0 },
                       "end": { "line": 4, "character": 10 } },
            "severity": 2,
            "message": "too long",
        });
        assert_eq!(
            diag_trail(&diag),
            Some((5, "yamllint/line-length".to_string())),
            "0-based range line maps to the 1-based hit line",
        );

        let codeless = serde_json::json!({
            "source": "mytool",
            "range": { "start": { "line": 0, "character": 0 },
                       "end": { "line": 0, "character": 1 } },
        });
        assert_eq!(
            diag_trail(&codeless),
            Some((1, "mytool".to_string())),
            "an absent code degrades to the source alone",
        );
    }
}
