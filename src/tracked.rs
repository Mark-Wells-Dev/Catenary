// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! "Tracked beats hidden": the git tracked-set consultation the search walks
//! run before skipping a dot-leading path (misc 227).
//!
//! # The rule
//!
//! Search skips hidden (dot-leading) paths by default — deliberate posture, and
//! right for `.env`, `.cache/`, `.venv/`. It is wrong for `.github/`,
//! `.cargo/config.toml`, `.config/` — directories a repository *tracks*, whose
//! contents an agent is routinely sent to edit. A default `catenary grep
//! workflow_dispatch` on this repo answered 6 matches in 5 files where
//! `--include-hidden` answered 9 in 7, and the missing two were tracked CI
//! config: "no matches" was indistinguishable from absence.
//!
//! Grep is already gitignore-aware, so it already asks git what the user cares
//! about. The maintainer ruling (misc 227, 2026-07-31) settles the tension the
//! same way: **a hidden path that git tracks is searched and listed by default;
//! untracked hidden stays skipped; `--include-hidden` remains the escape for the
//! rest.** A root with no enclosing repository has no tracked set, so the plain
//! hidden rule stands there unchanged.
//!
//! # Where the rule binds
//!
//! [`apply_hidden_posture`] is the one seam, shared by every walk that carries
//! search scope: `grep`'s hit walk, `glob`'s pattern expansion / listing /
//! child-count walks, and the diagnostics stat-walk whose observation set must
//! stay coverage-symmetric with grep's (an asymmetry would phantom-reap a live
//! file as `Deleted`).
//!
//! It turns the `ignore` crate's own hidden filter **off** and installs a
//! `filter_entry` gate in its place, so the decision is ours rather than the
//! crate's. `filter_entry` runs *after* the crate's gitignore / type / override
//! checks (`ignore::Walk::skip_entry`), which is exactly the ordering the rule
//! wants: gitignore is a separate axis with its own lever
//! (`--include-gitignored`), and a gitignored hidden path stays gitignored. The
//! walk **root** is exempt from every filter by the crate itself (`depth() == 0`
//! short-circuits `skip_entry`), so explicitly naming a hidden directory keeps
//! working as it always has (misc 45).
//!
//! # What "tracked" means for a directory
//!
//! Git tracks files, not directories, so a dot-leading *directory* is admitted
//! when it has at least one tracked path beneath it — otherwise descending it
//! could never reach tracked content. Once a hidden directory is admitted the
//! ordinary rules resume inside it: a newly written, not-yet-added
//! `.github/workflows/new.yml` is searched (the alternative — per-file
//! trackedness — would make an agent's own uncommitted work invisible to the
//! search that agent runs next), while a dot-leading entry nested deeper
//! (`.github/.secret`) faces the gate again on its own.
//!
//! # Cost bounding
//!
//! The `ignore` crate does not read the git index, so trackedness costs a real
//! consultation. The shape chosen is a **per-repository snapshot, built lazily,
//! cached for the life of one operation**:
//!
//! - **Lazy.** Nothing is spawned until a walk meets its first dot-leading
//!   entry. `.git` — the one hidden entry *every* repo-root walk meets — is
//!   refused by name *before* the snapshot is consulted, so its presence alone
//!   never triggers the build.
//! - **Once per repository per operation.** One [`TrackedHidden`] is created per
//!   search operation and shared by every root that operation walks, so a
//!   multi-root `grep pat src tests` over one repository spawns one
//!   `git ls-files`, not one per root.
//! - **No cache to go stale.** The instance is dropped with the operation.
//!   Staleness tolerance is therefore the operation's own duration — a
//!   long-lived daemon never serves a walk from a snapshot an earlier walk
//!   built, so there is no invalidation problem to get wrong.
//! - **Narrow.** `git ls-files -z --cached` reads the index without refreshing
//!   it (`GIT_OPTIONAL_LOCKS=0` — no lock, no write), and only tracked paths
//!   bearing a dot-leading component survive ingest. Measured on this
//!   repository: 392 tracked paths in, 34 admitted entries out, 5.6 ms for the
//!   whole build (debug; the term that scales is the `git` spawn, not the
//!   parse).
//! - **Free on the common path.** A walk with `--include-hidden`, a named-file
//!   root, or no enclosing repository never spawns anything; a non-hidden entry
//!   costs one leading-byte comparison. The no-hidden-tracked-files case a
//!   walk *does* pay for is one spawn — the snapshot comes back empty and every
//!   hidden path is skipped exactly as before.
//!
//! Degradation is logged-but-silent and always *toward the previous behavior*:
//! no `git` on `PATH`, a non-zero exit, an unreadable index — the tracked set is
//! empty and every hidden path keeps the plain skip.
//!
//! # Residuals
//!
//! - A submodule's own index is not read: the enclosing repository's `ls-files`
//!   reports the gitlink, not the submodule's files, so hidden paths *inside* a
//!   submodule keep the plain skip.
//! - "Hidden" here is the dot-leading name, matching `ignore`'s Unix rule
//!   (`is_hidden_path_only`) and Catenary's own
//!   (`ResolvedGlob::targets_hidden`, the glob zero-match disclosure). The
//!   Windows `FILE_ATTRIBUTE_HIDDEN` leg of `ignore`'s definition is not
//!   reproduced.
//! - Tracked-**and**-gitignored (a force-added path) stays gitignored: the
//!   ruling covers the hidden axis, and `--include-gitignored` is the other
//!   axis's lever.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use ignore::WalkBuilder;

/// The tracked-set consultation shared by the walks of one search operation.
///
/// One instance per operation (one `catenary grep`, one `catenary glob`, one
/// diagnostics stat-walk), shared by every root that operation walks: the
/// snapshot for a repository is built at most once per instance. The instance is
/// dropped with the operation, which is what bounds staleness — see the module
/// docs' cost-bounding section.
#[derive(Default)]
pub struct TrackedHidden {
    /// Repository root → the admitted path set, built on first ask.
    snapshots: Mutex<HashMap<PathBuf, Arc<HashSet<PathBuf>>>>,
}

impl TrackedHidden {
    /// A fresh consultation with no snapshot loaded.
    ///
    /// Returned behind an [`Arc`] because the gate installed on a walker must be
    /// `Send + Sync + 'static`, and one operation's roots share the instance.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The admitted set for `repo`, built once and reused thereafter.
    fn admitted(&self, repo: &Path) -> Arc<HashSet<PathBuf>> {
        let mut snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let snapshot = Arc::clone(
            snapshots
                .entry(repo.to_path_buf())
                .or_insert_with(|| Arc::new(load_admitted(repo))),
        );
        drop(snapshots);
        snapshot
    }
}

/// One walk root's view of the rule: which repository governs it, and that
/// repository's admitted set once anything has needed it.
struct Gate {
    /// The enclosing repository root, or `None` for a non-git root (where the
    /// plain hidden rule stands and nothing is ever consulted).
    repo: Option<PathBuf>,
    /// The operation-wide consultation this gate draws its snapshot from.
    tracked: Arc<TrackedHidden>,
    /// This root's snapshot, resolved on the first dot-leading entry that is not
    /// `.git`. One lock acquisition per walk root, not per entry.
    admitted: OnceLock<Arc<HashSet<PathBuf>>>,
}

impl Gate {
    /// Whether a dot-leading `path` is admitted into the walk anyway.
    fn admits(&self, path: &Path) -> bool {
        // The repository's own metadata directory is never tracked and is never
        // searchable content. Refusing it by name — *before* the snapshot is
        // touched — is what keeps the consultation lazy: `.git` is the one
        // hidden entry every repo-root walk meets, so consulting the tracked set
        // for it would make "lazy" mean "always".
        if path.file_name() == Some(OsStr::new(".git")) {
            return false;
        }
        let Some(repo) = self.repo.as_deref() else {
            // No enclosing repository: no tracked set exists, so the plain
            // hidden rule stands (the ruling's non-git carve-out).
            return false;
        };
        self.admitted
            .get_or_init(|| self.tracked.admitted(repo))
            .contains(path)
    }
}

/// Installs the "tracked beats hidden" posture on `builder` for a walk rooted at
/// `root`.
///
/// Replaces the `.hidden(skip_hidden)` call every search walk used to make.
/// `skip_hidden == false` (`--include-hidden`) is byte-for-byte the old
/// behavior: the filter is never installed, so the escape hatch reaches
/// everything it always did, `.git` included.
///
/// `tracked` is the operation-wide consultation (see [`TrackedHidden::new`]);
/// pass the same one to every root of a single `grep`/`glob`/stat-walk so the
/// repository snapshot is built at most once.
pub fn apply_hidden_posture(
    builder: &mut WalkBuilder,
    root: &Path,
    skip_hidden: bool,
    tracked: &Arc<TrackedHidden>,
) {
    if !skip_hidden {
        builder.hidden(false);
        return;
    }
    let gate = Gate {
        repo: enclosing_repo(root),
        tracked: Arc::clone(tracked),
        admitted: OnceLock::new(),
    };
    // The crate's hidden filter is off so the decision reaches our gate at all;
    // the gate then refuses exactly what the crate would have refused, minus the
    // paths git tracks. `filter_entry` runs after the crate's gitignore checks,
    // so a gitignored hidden path never even reaches the gate.
    builder.hidden(false).filter_entry(move |entry| {
        let path = entry.path();
        !is_hidden_name(path) || gate.admits(path)
    });
}

/// Whether `path`'s own final component is hidden (dot-leading).
///
/// Byte-compares the leading `.` exactly as `ignore`'s own Unix rule does, so a
/// non-UTF-8 dotfile is classified identically rather than falling through as
/// visible.
#[must_use]
pub fn is_hidden_name(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
}

/// The repository root governing `root`, or `None` outside a repository.
///
/// Walks up from `root` (inclusive) for a `.git` entry — a directory for a
/// normal checkout, a file for a linked worktree or submodule; `git` itself
/// reads both. The nearest marker wins, so a submodule or worktree is governed
/// by its own index rather than a superproject's.
fn enclosing_repo(root: &Path) -> Option<PathBuf> {
    // A named-file root is governed by the repository its directory sits in.
    let start = if root.is_file() { root.parent()? } else { root };
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Builds `repo`'s admitted set: every tracked path bearing a dot-leading
/// component, plus the ancestor directories a walk must descend to reach it.
///
/// Filtering to dot-bearing paths at ingest is what keeps the set small — the
/// overwhelming majority of a repository's tracked paths are irrelevant to a
/// rule that only ever fires on a dot-leading entry.
fn load_admitted(repo: &Path) -> HashSet<PathBuf> {
    let mut admitted = HashSet::new();
    let Some(stdout) = ls_files(repo) else {
        tracing::debug!(
            "tracked-hidden: no tracked set for {} — hidden paths keep the plain skip",
            repo.display()
        );
        return admitted;
    };
    // `-z` output is NUL-separated and unquoted, so no unescaping is needed. A
    // non-UTF-8 path is skipped rather than poisoning the whole set: it keeps
    // the plain skip, one path's worth of degradation.
    for record in stdout.split(|byte| *byte == 0) {
        if let Ok(rel) = std::str::from_utf8(record) {
            insert_hidden_chain(repo, rel, &mut admitted);
        }
    }
    admitted
}

/// Records `rel` and its ancestor directories under `repo` — but only when some
/// component of `rel` is dot-leading.
fn insert_hidden_chain(repo: &Path, rel: &str, admitted: &mut HashSet<PathBuf>) {
    let segments = || rel.split('/').filter(|segment| !segment.is_empty());
    if !segments().any(is_hidden_segment) {
        return;
    }
    let mut cursor = repo.to_path_buf();
    for segment in segments() {
        cursor.push(segment);
        admitted.insert(cursor.clone());
    }
}

/// Whether one path segment is hidden, excluding the trivial navigation
/// components (which a `git ls-files` path never contains, but which would be
/// wrong to treat as hidden if one ever did).
fn is_hidden_segment(segment: &str) -> bool {
    segment.starts_with('.') && segment != "." && segment != ".."
}

/// Reads `repo`'s index, returning the raw NUL-separated tracked paths.
///
/// Read-only and lock-free: `--cached` reads the index without refreshing it and
/// `GIT_OPTIONAL_LOCKS=0` keeps git from taking a lock to write one back, so a
/// search can never contend with — or disturb — a concurrent git operation. Any
/// failure (git missing, `PATH` empty, not a repository, an unreadable index,
/// non-zero exit) is `None`, and the caller degrades to the plain hidden rule.
fn ls_files(repo: &Path) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--cached"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// A repository with a tracked hidden tree (`.alpha`), an untracked hidden
    /// sibling (`.beta`), and a visible tree.
    fn repo_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".alpha/sub")).expect("mk .alpha/sub");
        std::fs::create_dir_all(root.join(".beta")).expect("mk .beta");
        std::fs::create_dir_all(root.join("vis")).expect("mk vis");
        std::fs::write(root.join(".alpha/a.txt"), "needle\n").expect("write a");
        std::fs::write(root.join(".alpha/sub/deep.txt"), "needle\n").expect("write deep");
        std::fs::write(root.join(".beta/b.txt"), "needle\n").expect("write b");
        std::fs::write(root.join("vis/v.txt"), "needle\n").expect("write v");
        git(root, &["init", "-q"]);
        git(root, &["add", ".alpha", "vis"]);
        dir
    }

    /// Runs a git command in `root`, ignoring output.
    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} should succeed");
    }

    #[test]
    fn hidden_name_matches_the_dot_leading_rule() {
        assert!(is_hidden_name(Path::new("/repo/.github")));
        assert!(is_hidden_name(Path::new("/repo/.gitignore")));
        assert!(!is_hidden_name(Path::new("/repo/src")));
        assert!(!is_hidden_name(Path::new("/repo/src/main.rs")));
    }

    #[test]
    fn hidden_chain_records_ancestors_of_a_dotted_path() {
        let mut admitted = HashSet::new();
        insert_hidden_chain(
            Path::new("/repo"),
            ".github/workflows/ci.yml",
            &mut admitted,
        );
        assert!(admitted.contains(Path::new("/repo/.github")));
        assert!(admitted.contains(Path::new("/repo/.github/workflows")));
        assert!(admitted.contains(Path::new("/repo/.github/workflows/ci.yml")));
    }

    #[test]
    fn hidden_chain_ignores_a_path_with_no_dotted_component() {
        let mut admitted = HashSet::new();
        insert_hidden_chain(Path::new("/repo"), "src/main.rs", &mut admitted);
        assert!(
            admitted.is_empty(),
            "a fully visible path costs the set nothing: {admitted:?}"
        );
    }

    #[test]
    fn hidden_chain_records_a_dotted_component_below_a_visible_parent() {
        let mut admitted = HashSet::new();
        insert_hidden_chain(Path::new("/repo"), "src/.cargo/config.toml", &mut admitted);
        assert!(admitted.contains(Path::new("/repo/src/.cargo")));
        assert!(admitted.contains(Path::new("/repo/src/.cargo/config.toml")));
    }

    #[test]
    fn enclosing_repo_finds_a_dot_git_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mk .git");
        std::fs::create_dir_all(dir.path().join("src")).expect("mk src");
        assert_eq!(
            enclosing_repo(&dir.path().join("src")).as_deref(),
            Some(dir.path())
        );
    }

    #[test]
    fn enclosing_repo_finds_a_dot_git_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").expect("write .git");
        assert_eq!(enclosing_repo(dir.path()).as_deref(), Some(dir.path()));
    }

    #[test]
    fn enclosing_repo_is_none_outside_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(enclosing_repo(dir.path()), None);
    }

    #[test]
    fn tracked_hidden_paths_are_admitted_and_untracked_ones_are_not() {
        let dir = repo_fixture();
        let root = dir.path();
        let admitted = load_admitted(root);
        assert!(
            admitted.contains(&root.join(".alpha")),
            "a hidden dir with tracked content is admitted: {admitted:?}"
        );
        assert!(admitted.contains(&root.join(".alpha/sub/deep.txt")));
        assert!(
            !admitted.contains(&root.join(".beta")),
            "an untracked hidden dir stays skipped: {admitted:?}"
        );
        assert!(
            !admitted.contains(&root.join("vis")),
            "a fully visible tracked path never enters the set: {admitted:?}"
        );
    }

    #[test]
    fn gate_refuses_dot_git_without_consulting_the_snapshot() {
        let dir = repo_fixture();
        let tracked = TrackedHidden::new();
        let gate = Gate {
            repo: Some(dir.path().to_path_buf()),
            tracked: Arc::clone(&tracked),
            admitted: OnceLock::new(),
        };
        assert!(!gate.admits(&dir.path().join(".git")));
        // The refusal is by name, so nothing was loaded — the laziness that
        // keeps a repo-root walk from paying for the snapshot it may not need.
        assert!(
            gate.admitted.get().is_none(),
            "`.git` must be refused before the snapshot is built"
        );
    }

    #[test]
    fn gate_outside_a_repository_admits_nothing_and_loads_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tracked = TrackedHidden::new();
        let gate = Gate {
            repo: enclosing_repo(dir.path()),
            tracked: Arc::clone(&tracked),
            admitted: OnceLock::new(),
        };
        assert!(!gate.admits(&dir.path().join(".anything")));
        assert!(
            gate.admitted.get().is_none(),
            "a non-git root never consults a tracked set"
        );
    }

    #[test]
    fn one_snapshot_serves_every_root_of_an_operation() {
        let dir = repo_fixture();
        let tracked = TrackedHidden::new();
        let first = tracked.admitted(dir.path());
        let second = tracked.admitted(dir.path());
        assert!(
            Arc::ptr_eq(&first, &second),
            "a repository's snapshot is built once per operation"
        );
    }

    #[test]
    fn walk_admits_tracked_hidden_and_still_skips_untracked_hidden() {
        let dir = repo_fixture();
        let root = dir.path();
        let tracked = TrackedHidden::new();
        let mut builder = WalkBuilder::new(root);
        builder.git_ignore(true);
        apply_hidden_posture(&mut builder, root, true, &tracked);
        let seen: HashSet<PathBuf> = builder
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .collect();

        assert!(
            seen.contains(&root.join(".alpha/a.txt")),
            "tracked hidden content joins the default walk: {seen:?}"
        );
        assert!(seen.contains(&root.join(".alpha/sub/deep.txt")));
        assert!(
            !seen.contains(&root.join(".beta/b.txt")),
            "untracked hidden content stays skipped: {seen:?}"
        );
        assert!(
            !seen.contains(&root.join(".git")),
            "the repository's own metadata dir is never walked: {seen:?}"
        );
        assert!(seen.contains(&root.join("vis/v.txt")));
    }

    #[test]
    fn walk_outside_a_repository_keeps_the_plain_hidden_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".alpha")).expect("mk .alpha");
        std::fs::write(root.join(".alpha/a.txt"), "needle\n").expect("write a");
        std::fs::write(root.join("v.txt"), "needle\n").expect("write v");

        let tracked = TrackedHidden::new();
        let mut builder = WalkBuilder::new(root);
        builder.git_ignore(true);
        apply_hidden_posture(&mut builder, root, true, &tracked);
        let seen: HashSet<PathBuf> = builder
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .collect();

        assert!(
            !seen.contains(&root.join(".alpha/a.txt")),
            "a non-git root has no tracked set, so hidden stays hidden: {seen:?}"
        );
        assert!(seen.contains(&root.join("v.txt")));
    }

    #[test]
    fn include_hidden_still_reaches_untracked_hidden_paths() {
        let dir = repo_fixture();
        let root = dir.path();
        let tracked = TrackedHidden::new();
        let mut builder = WalkBuilder::new(root);
        builder.git_ignore(true);
        apply_hidden_posture(&mut builder, root, false, &tracked);
        let seen: HashSet<PathBuf> = builder
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .collect();

        assert!(
            seen.contains(&root.join(".beta/b.txt")),
            "`--include-hidden` is unchanged — it still reaches untracked hidden: {seen:?}"
        );
    }

    #[test]
    fn an_admitted_hidden_dir_still_gates_a_dotted_child() {
        let dir = repo_fixture();
        let root = dir.path();
        std::fs::write(root.join(".alpha/.secret"), "shh\n").expect("write secret");
        let tracked = TrackedHidden::new();
        let mut builder = WalkBuilder::new(root);
        builder.git_ignore(true);
        apply_hidden_posture(&mut builder, root, true, &tracked);
        let seen: HashSet<PathBuf> = builder
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .collect();

        assert!(
            !seen.contains(&root.join(".alpha/.secret")),
            "descending an admitted hidden dir does not lift the rule for its own \
             dot-leading children: {seen:?}"
        );
    }

    #[test]
    fn an_admitted_hidden_dir_surfaces_a_not_yet_added_file() {
        let dir = repo_fixture();
        let root = dir.path();
        std::fs::write(root.join(".alpha/fresh.txt"), "needle\n").expect("write fresh");
        let tracked = TrackedHidden::new();
        let mut builder = WalkBuilder::new(root);
        builder.git_ignore(true);
        apply_hidden_posture(&mut builder, root, true, &tracked);
        let seen: HashSet<PathBuf> = builder
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .collect();

        assert!(
            seen.contains(&root.join(".alpha/fresh.txt")),
            "an agent's own uncommitted file in an admitted hidden dir is searchable: {seen:?}"
        );
    }

    #[test]
    fn a_gitignored_hidden_dir_stays_gitignored() {
        let dir = repo_fixture();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".cache")).expect("mk .cache");
        std::fs::write(root.join(".cache/c.txt"), "needle\n").expect("write c");
        std::fs::write(root.join(".gitignore"), ".cache/\n").expect("write gitignore");
        git(root, &["add", "-f", ".gitignore", ".cache/c.txt"]);

        let tracked = TrackedHidden::new();
        let mut builder = WalkBuilder::new(root);
        builder.git_ignore(true);
        apply_hidden_posture(&mut builder, root, true, &tracked);
        let seen: HashSet<PathBuf> = builder
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .collect();

        assert!(
            !seen.contains(&root.join(".cache/c.txt")),
            "gitignore is the other axis and keeps its own lever: {seen:?}"
        );
        assert!(
            seen.contains(&root.join(".gitignore")),
            "a tracked, non-ignored dotfile is admitted: {seen:?}"
        );
    }
}
