// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CLI-side lint routing (ws43-04): which linter covers which file, with **no
//! daemon in the loop**.
//!
//! The routing rules are the shared ones documented on [`crate::linter`]: the
//! per-root effective set is [`merge_effective_linters`] over the user config
//! and the root's `.catenary.toml`, gated by the root's `[linter] disable`,
//! and the per-file predicate is
//! [`FilesystemManager::linter_routes`] (path glob OR shebang). The one thing
//! the CLI resolves differently is the **root itself**: without the daemon's
//! registered-roots ledger, a file's owning root is its enclosing worktree
//! root ([`enclosing_worktree_root`]) — the same discovery the daemon's query
//! auto-mount performs, so both sides answer the same root for any file inside
//! a repository. A file outside every repository resolves no root and is not
//! lint-covered, mirroring the daemon feeder's resolve-or-skip rule.
//!
//! Discovery and project-config loads are memoized per query: one ancestor
//! walk per distinct parent directory, one `.catenary.toml` load per root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bridge::filesystem_manager::FilesystemManager;
use crate::companions::enclosing_worktree_root;
use crate::config::LinterConfig;

use super::merge_effective_linters;

/// One linter run the router planned: a named linter, the root it runs under,
/// and the files it covers there.
pub struct LintJob {
    /// The `[linter.rule.<name>]` key — selects the parse adapter.
    pub name: String,
    /// The effective (merged) linter config.
    pub linter: LinterConfig,
    /// The owning root — relative reported paths resolve against it.
    pub root: PathBuf,
    /// The covered files, in input order.
    pub files: Vec<PathBuf>,
}

/// The enabled, effective linter set for one discovered root, sorted by name
/// for deterministic run order (the daemon feeder's ordering rule).
struct RootLinters {
    /// Enabled entries only (`disable` and empty-command entries dropped), in
    /// name order.
    linters: Vec<(String, LinterConfig)>,
}

/// Per-file lint coverage and run planning for the CLI query sink.
///
/// Holds the user `[linter.rule.*]` layer and a [`FilesystemManager`] (the
/// shebang-read cache behind [`FilesystemManager::linter_routes`]); discovers
/// each file's enclosing worktree root and loads its project overlay lazily,
/// memoized for the life of the router (one query).
pub struct LintRouter {
    /// The user config's `[linter.rule.*]` layer.
    user: HashMap<String, LinterConfig>,
    /// Shebang classification cache behind the shared routing predicate.
    fs: Arc<FilesystemManager>,
    /// Root path → its effective enabled linter set.
    roots: HashMap<PathBuf, Arc<RootLinters>>,
    /// Parent directory → discovered enclosing root (negative results cached
    /// too, so a tree outside any repository costs one ancestor walk per dir).
    dirs: HashMap<PathBuf, Option<PathBuf>>,
}

impl LintRouter {
    /// Builds a router over the user linter layer and the shared filesystem
    /// classification cache.
    #[must_use]
    pub fn new(user: HashMap<String, LinterConfig>, fs: Arc<FilesystemManager>) -> Self {
        Self {
            user,
            fs,
            roots: HashMap::new(),
            dirs: HashMap::new(),
        }
    }

    /// Whether any enabled linter covers `file` — the per-hit routing
    /// predicate: `true` sends the hit to the local linter sink, never the
    /// daemon.
    pub fn covers(&mut self, file: &Path) -> bool {
        let Some((root, linters)) = self.resolve(file) else {
            return false;
        };
        let Ok(rel) = file.strip_prefix(&root).map(Path::to_path_buf) else {
            return false;
        };
        linters
            .linters
            .iter()
            .any(|(_, linter)| self.fs.linter_routes(linter, file, &rel))
    }

    /// Plans the linter runs for a set of lint-covered files: grouped by owning
    /// root, then by linter in name order (the daemon feeder's deterministic
    /// grouping), each job carrying the files its linter routes.
    ///
    /// Files that resolve no root or match no enabled linter simply plan
    /// nothing — the caller's coverage mask should have filtered them already.
    pub fn plan(&mut self, files: &[PathBuf]) -> Vec<LintJob> {
        // Group by root, preserving input order within each group.
        let mut by_root: Vec<(PathBuf, Arc<RootLinters>, Vec<PathBuf>)> = Vec::new();
        for file in files {
            let Some((root, linters)) = self.resolve(file) else {
                continue;
            };
            if let Some(entry) = by_root.iter_mut().find(|(r, _, _)| *r == root) {
                entry.2.push(file.clone());
            } else {
                by_root.push((root, linters, vec![file.clone()]));
            }
        }

        let mut jobs = Vec::new();
        for (root, linters, root_files) in by_root {
            for (name, linter) in &linters.linters {
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
                jobs.push(LintJob {
                    name: name.clone(),
                    linter: linter.clone(),
                    root: root.clone(),
                    files: matching,
                });
            }
        }
        jobs
    }

    /// Resolves a file's owning root and that root's enabled linter set,
    /// memoized. `None` when the file lies outside every repository or its
    /// root's effective set is empty (including `[linter] disable = true`).
    fn resolve(&mut self, file: &Path) -> Option<(PathBuf, Arc<RootLinters>)> {
        let dir = file.parent()?.to_path_buf();
        let root = self
            .dirs
            .entry(dir)
            .or_insert_with_key(|d| enclosing_worktree_root(d))
            .clone()?;
        let linters = if let Some(known) = self.roots.get(&root) {
            Arc::clone(known)
        } else {
            let loaded = Arc::new(self.load_root(&root));
            self.roots.insert(root.clone(), Arc::clone(&loaded));
            loaded
        };
        if linters.linters.is_empty() {
            return None;
        }
        Some((root, linters))
    }

    /// Loads a root's project overlay and computes its enabled effective set.
    ///
    /// A missing or malformed `.catenary.toml` degrades to the user layer alone
    /// (the same fallback the daemon's `Root::load` applies), so a broken
    /// project config never breaks routing.
    fn load_root(&self, root: &Path) -> RootLinters {
        let project = crate::config::load_project_config(root)
            .ok()
            .flatten()
            .unwrap_or_default();
        if project.disable_lint {
            return RootLinters {
                linters: Vec::new(),
            };
        }
        let merged = merge_effective_linters(&self.user, &project.linter);
        let mut linters: Vec<(String, LinterConfig)> = merged
            .into_iter()
            .filter(|(_, linter)| !linter.disable && !linter.command.is_empty())
            .collect();
        linters.sort_by(|a, b| a.0.cmp(&b.0));
        RootLinters { linters }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// A tempdir carrying a `.git` marker so root discovery resolves it.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("git marker");
        dir
    }

    fn shellcheck_layer() -> HashMap<String, LinterConfig> {
        let linter =
            LinterConfig::new("shellcheck", vec![], vec!["**/*.sh".to_string()]).expect("compile");
        std::iter::once(("shellcheck".to_string(), linter)).collect()
    }

    #[test]
    fn covers_by_pattern_inside_a_repo_and_plans_the_run() {
        let repo = repo();
        let root = repo.path().canonicalize().expect("canonical root");
        let sh = root.join("scripts/build.sh");
        std::fs::create_dir_all(sh.parent().expect("parent")).expect("mkdir");
        std::fs::write(&sh, "echo hi\n").expect("write");
        let rs = root.join("src.rs");
        std::fs::write(&rs, "fn main() {}\n").expect("write");

        let mut router = LintRouter::new(shellcheck_layer(), Arc::new(FilesystemManager::new()));
        assert!(
            router.covers(&sh),
            "a .sh under the repo routes to shellcheck"
        );
        assert!(!router.covers(&rs), "a .rs matches no routing glob");

        let jobs = router.plan(std::slice::from_ref(&sh));
        assert_eq!(jobs.len(), 1, "one job for the one covering linter");
        assert_eq!(jobs[0].name, "shellcheck");
        assert_eq!(jobs[0].root, root, "the job runs under the discovered root");
        assert_eq!(jobs[0].files, vec![sh]);
    }

    #[test]
    fn outside_any_repository_is_not_covered() {
        // No `.git` marker anywhere under the tempdir: discovery may still find
        // an enclosing repo ABOVE the tempdir on some machines, so pin the
        // negative with a file whose ancestors are the tempdir chain only.
        let dir = tempfile::tempdir().expect("tempdir");
        let sh = dir.path().join("loose.sh");
        std::fs::write(&sh, "echo hi\n").expect("write");
        let mut router = LintRouter::new(shellcheck_layer(), Arc::new(FilesystemManager::new()));
        // The system temp dir is outside any repository in practice; if an
        // enclosing repo exists the assertion below would be environmental, so
        // assert only when discovery finds nothing (the common case).
        if enclosing_worktree_root(&sh).is_none() {
            assert!(!router.covers(&sh), "no enclosing root, no lint coverage");
        }
    }

    #[test]
    fn project_overlay_disables_by_name_and_root_toggle_drops_all() {
        // Root A disables shellcheck by name; root B turns lint off wholesale.
        let repo_a = repo();
        let root_a = repo_a.path().canonicalize().expect("canonical");
        std::fs::write(
            root_a.join(".catenary.toml"),
            "[linter.rule.shellcheck]\ndisable = true\n",
        )
        .expect("write project config");
        let sh_a = root_a.join("build.sh");
        std::fs::write(&sh_a, "echo hi\n").expect("write");

        let repo_b = repo();
        let root_b = repo_b.path().canonicalize().expect("canonical");
        std::fs::write(root_b.join(".catenary.toml"), "[linter]\ndisable = true\n")
            .expect("write project config");
        let sh_b = root_b.join("build.sh");
        std::fs::write(&sh_b, "echo hi\n").expect("write");

        let mut router = LintRouter::new(shellcheck_layer(), Arc::new(FilesystemManager::new()));
        assert!(
            !router.covers(&sh_a),
            "a project entry disables the user linter by name",
        );
        assert!(
            !router.covers(&sh_b),
            "[linter] disable drops the whole set for the root",
        );
    }

    #[test]
    fn plan_groups_files_under_one_job_per_linter() {
        let repo = repo();
        let root = repo.path().canonicalize().expect("canonical");
        let a = root.join("a.sh");
        let b = root.join("b.sh");
        std::fs::write(&a, "echo a\n").expect("write");
        std::fs::write(&b, "echo b\n").expect("write");

        let mut router = LintRouter::new(shellcheck_layer(), Arc::new(FilesystemManager::new()));
        let jobs = router.plan(&[a.clone(), b.clone()]);
        assert_eq!(jobs.len(), 1, "both files ride one shellcheck run");
        assert_eq!(jobs[0].files, vec![a, b], "input order preserved");
    }
}
