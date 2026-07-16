// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared application container for tool servers and cross-tool infrastructure.
//!
//! `Session` creates and owns all internal servers and shared dependencies.
//! Protocol boundaries (`LspBridgeHandler`, `HookServer`) hold `Arc<Session>`
//! and access any dependency through it.

use anyhow::Result;
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::diagnostics_server::DiagnosticsServer;
use super::editing_guardrail::EditingGuardrail;
use super::editing_manager::EditingManager;
use super::file_tools::GlobServer;
use super::filesystem_manager::{FilesystemManager, Root};
use super::grep_server::GrepServer;
use super::handler::expand_tilde;
use super::path_security::PathValidator;
use crate::config::Config;
use crate::logging::LoggingServer;
use crate::logging::jsonl_sink::JsonlSink;
use crate::lsp::LspClientManager;
use crate::lsp::glob::LspGlob;
use crate::symbol_index::SymbolIndex;

/// A resolved glob pattern that handles tilde expansion and absolute paths.
///
/// For relative patterns (e.g. `src/**/*.rs`), matches against paths relative
/// to workspace roots. For absolute patterns (e.g. `~/other-project/*.rs`),
/// extracts the non-glob base directory as a search root and matches against
/// full paths.
///
/// `Clone` so it can move into the `spawn_blocking` task that runs glob's
/// off-thread directory walks (misc 140 phase 2).
#[derive(Clone)]
pub struct ResolvedGlob {
    glob: LspGlob,
    match_full_path: bool,
    override_root: Option<PathBuf>,
    /// The deepest a matching path can lie below [`override_root`], used to
    /// bound the expansion walk. `Some(n)` when every pattern segment after the
    /// metachar-free base is a single-component glob (`literal_separator(true)`
    /// means `*`/`?`/`[]` never cross `/`, so each such segment consumes exactly
    /// one path component); `None` when a `**` segment makes the depth unbounded.
    /// Only meaningful for absolute patterns (an `override_root` walk); relative
    /// patterns carry no base and never walk.
    max_depth: Option<usize>,
}

impl ResolvedGlob {
    /// Resolves a glob pattern, expanding tilde and detecting absolute patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid glob.
    pub fn new(pattern: &str) -> Result<Self> {
        let expanded = expand_tilde(pattern);
        let glob = LspGlob::new(&expanded)?;

        if Path::new(&expanded).is_absolute() {
            let base = Self::base_dir(&expanded);
            let max_depth = Self::match_depth(&expanded, &base);
            Ok(Self {
                glob,
                match_full_path: true,
                override_root: Some(base),
                max_depth,
            })
        } else {
            Ok(Self {
                glob,
                match_full_path: false,
                override_root: None,
                max_depth: None,
            })
        }
    }

    /// Tests whether a file path matches this glob.
    ///
    /// For absolute patterns, matches against the full path.
    /// For relative patterns, strips the root prefix first.
    #[must_use]
    pub fn is_match(&self, path: &Path, root: &Path) -> bool {
        if self.match_full_path {
            self.glob.is_match(path)
        } else {
            let rel = path.strip_prefix(root).unwrap_or(path);
            self.glob.is_match(rel)
        }
    }

    /// Returns the override search root for absolute patterns.
    #[must_use]
    pub fn override_root(&self) -> Option<&Path> {
        self.override_root.as_deref()
    }

    /// Returns `true` if the pattern explicitly targets hidden files.
    ///
    /// A pattern is "explicit" when any path segment starts with `.`
    /// (excluding the trivial `.` and `..` navigation components).
    /// Examples: `.gitignore`, `.github/*.yml`, `.git*`.
    ///
    /// When this returns `true`, callers should force `include_hidden`
    /// so that the directory walker does not skip the targeted entries.
    #[must_use]
    pub fn targets_hidden(pattern: &str) -> bool {
        pattern
            .split('/')
            .any(|seg| seg.starts_with('.') && seg != "." && seg != "..")
    }

    /// Extracts the longest directory prefix without glob metacharacters.
    fn base_dir(pattern: &str) -> PathBuf {
        let mut base = PathBuf::new();
        for component in Path::new(pattern).components() {
            let s = component.as_os_str().to_string_lossy();
            if s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{') {
                break;
            }
            base.push(component);
        }
        if base.as_os_str().is_empty() {
            PathBuf::from("/")
        } else {
            base
        }
    }

    /// The deepest a match can lie below `base`, or `None` if a `**` segment
    /// makes it unbounded.
    ///
    /// `base` is the pattern's metachar-free prefix ([`base_dir`](Self::base_dir)),
    /// so the segments *after* it are the glob part. Each glob segment matches
    /// exactly one path component — `LspGlob` compiles with
    /// `literal_separator(true)`, so `*`/`?`/`[…]` never cross `/` — **except**
    /// `**`, which crosses segment boundaries and lifts the bound. The count of
    /// post-base segments is therefore the maximum depth below `base` at which a
    /// path can match; a `**` anywhere in the remainder returns `None`
    /// (unbounded). This bounds the expansion walk (misc 159): a single-star
    /// pattern like `/base/t*` need only enumerate `base`'s direct children
    /// rather than descend every sibling subtree.
    fn match_depth(pattern: &str, base: &Path) -> Option<usize> {
        let total = Path::new(pattern).components().count();
        let base_len = base.components().count();
        // Segments the glob part contributes below `base`.
        let remainder = total.saturating_sub(base_len);
        // A `**` component (or an embedded `**`) crosses `/`, so depth is
        // unbounded. Inspect only the post-base components.
        let unbounded = Path::new(pattern)
            .components()
            .skip(base_len)
            .any(|c| c.as_os_str().to_string_lossy().contains("**"));
        if unbounded { None } else { Some(remainder) }
    }

    /// Expands this glob into the concrete paths it matches on disk.
    ///
    /// Walks the pattern's non-glob base directory with the gitignore-aware
    /// [`ignore`] walker, so within a git repository gitignored and (by
    /// default) hidden entries are skipped — a blind `**/*.rs` from a project
    /// root would otherwise descend into `target/` and hang.
    /// `include_gitignored` / `include_hidden` lift those filters. Gitignore is
    /// repo-scoped (matching ripgrep and editors): outside a git repository no
    /// `.gitignore` rules apply. Results are sorted for deterministic output.
    ///
    /// Only meaningful for absolute patterns — the sole form path-argument
    /// expansion sees, because the daemon absolutizes every relative path
    /// argument against the request's `cwd` (in `GrepRequest`/`GlobRequest`
    /// `to_params`) before dispatch (bugs 31, 69). Relative patterns carry no
    /// base directory and yield an empty list; the relative form survives only
    /// for `--glob` scope filters, which match root-relative via
    /// [`is_match`](Self::is_match).
    #[must_use]
    pub fn expand(&self, include_gitignored: bool, include_hidden: bool) -> Vec<PathBuf> {
        self.expand_cancellable(
            include_gitignored,
            include_hidden,
            &CancellationToken::new(),
        )
    }

    /// Like [`expand`](Self::expand), but quits the base-directory walk the
    /// instant `cancel` fires (misc 140 phase 2).
    ///
    /// The walk is bounded to [`max_depth`](Self::max_depth) — the deepest a
    /// match can lie below the base (misc 159), so a single-star pattern like
    /// `/base/t*` enumerates only `base`'s direct children instead of descending
    /// every sibling's subtree (the bug-78 runaway: an unbounded walk made a
    /// hanging `t*` and an instant `d*` pay the *same* full base walk). A `**`
    /// pattern is unbounded and keeps the full recursive walk it needs.
    ///
    /// The walk is checked per entry so a massive base directory — the residual
    /// phase-1 named — stops mid-walk once the router's disconnect `select!`
    /// fires the token (the walk runs off the runtime thread via
    /// `spawn_blocking`). A cancelled walk yields no partial result: the caller
    /// discards it, exactly as a cancelled grep walk discards its matches.
    #[must_use]
    pub fn expand_cancellable(
        &self,
        include_gitignored: bool,
        include_hidden: bool,
        cancel: &CancellationToken,
    ) -> Vec<PathBuf> {
        let Some(base) = self.override_root.as_deref() else {
            return Vec::new();
        };
        let mut builder = WalkBuilder::new(base);
        builder
            .git_ignore(!include_gitignored)
            .hidden(!include_hidden);
        // Bound the walk to the deepest a match can lie (misc 159): a
        // single-star pattern (`/base/t*`) matches only `base`'s direct
        // children, so descending every sibling subtree is pure waste — the
        // runaway's contradiction 1 (the base walk was identical for a hanging
        // `t*` and an instant `d*`). A `**` pattern has `max_depth == None` and
        // keeps the full recursive walk it needs.
        if let Some(depth) = self.max_depth {
            builder.max_depth(Some(depth));
        }
        // A fully-literal pattern (`max_depth == Some(0)`: no glob part, so
        // `base` IS the exact target) roots the walk *at* the target, which the
        // `ignore` walker never gitignore-filters — its own root always yields.
        // For glob's always-pattern form that would be a metachar-free gitignore
        // bypass (VERBS: no metachar-free bypass; `--include-gitignored` is the
        // one lever), file or directory alike. Gate the root explicitly. The
        // *hidden* dimension is deliberately NOT gated here: an explicitly named
        // dot-leading target matches natively (misc 45 — the wildcard language
        // only refuses to *cross* a leading dot; an explicit name is not a
        // wildcard), so `include_hidden` governs only wildcard traversal.
        let gate_literal_gitignore = self.max_depth == Some(0) && !include_gitignored;
        let mut visible: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        let mut matches: Vec<PathBuf> = Vec::new();
        for entry in builder.build().flatten() {
            #[cfg(test)]
            crate::bridge::file_tools::probe::EXPAND_ENTRIES
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if cancel.is_cancelled() {
                return Vec::new();
            }
            let path = entry.into_path();
            if self.is_match(&path, base) {
                if gate_literal_gitignore && is_gitignored(&path, &mut visible) {
                    continue;
                }
                matches.push(path);
            }
        }
        matches.sort();
        matches
    }
}

/// A set of compiled `--exclude-pattern` globs, matched as a union.
///
/// `--exclude-pattern` is repeatable (clap append, like its siblings `--glob`
/// and `--type`): every occurrence contributes one pattern, and a path is
/// excluded when **any** pattern matches it. An empty set excludes nothing —
/// the no-exclude case is a set of zero patterns, not a wrapping `Option`, so a
/// single flat type reaches every consumer (bug 89) and no collected pattern
/// can be silently dropped by a leg that forgot to thread the `Option` (the
/// bug-73 leak class).
///
/// Exclude patterns are only ever *matched*, never expanded, so a
/// [`ResolvedGlob`] per pattern is sufficient. `Clone` so it can move into the
/// `spawn_blocking` tasks that run glob's off-thread walks (mirroring
/// [`ResolvedGlob`]).
#[derive(Clone, Default)]
pub struct ExcludeSet {
    globs: Vec<ResolvedGlob>,
}

impl ExcludeSet {
    /// Compiles each pattern into the set, skipping empty strings.
    ///
    /// Each pattern is compiled independently — the router has already resolved
    /// each against `cwd` (grep) or expanded a basename to `**/<name>` (glob),
    /// so per-pattern spellings differ and must not be merged into one glob.
    ///
    /// # Errors
    ///
    /// Returns an error if any pattern is not a valid glob.
    pub fn compile(patterns: &[String]) -> Result<Self> {
        let globs = patterns
            .iter()
            .filter(|p| !p.is_empty())
            .map(|p| ResolvedGlob::new(p))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { globs })
    }

    /// True when no pattern was supplied — excludes nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    /// Whether **any** pattern in the set selects `path` (matched against
    /// `root`), the union semantics of a repeated `--exclude-pattern`.
    #[must_use]
    pub fn is_match(&self, path: &Path, root: &Path) -> bool {
        self.globs.iter().any(|g| g.is_match(path, root))
    }
}

/// Resolves search path arguments into the concrete paths to scope a query.
///
/// Mirrors the CLI's literal-first contract on the daemon side: a path that
/// exists on disk (file, directory, or symlink — including a broken one) is
/// kept; a non-existent path is treated as a glob pattern and expanded via
/// [`ResolvedGlob::expand`]. A non-existent path with no glob metacharacters
/// compiles to a literal glob whose base directory does not exist, so it
/// expands to nothing — the CLI reports those as `path does not exist` before
/// they ever reach here.
///
/// An existing concrete **file** is searched/outlined unconditionally —
/// naming it is a direct request for that exact file, so the gitignore (and
/// hidden) gate does not apply (misc 110, ripgrep parity: ripgrep searches
/// files you name even when ignored). Existing **directories** are still
/// filtered against `.gitignore` unless `include_gitignored` is set: the gate
/// governs the recursive directory walk, not a named file. Gitignore is
/// repo-scoped (matching ripgrep/editors); outside a git repository nothing is
/// filtered.
///
/// Paths are expected to be absolute — the daemon absolutizes every relative
/// path argument against the request's `cwd` (in `GrepRequest`/`GlobRequest`
/// `to_params`) before dispatch. An empty input yields an empty result; callers
/// distinguish "no path arguments" (search `cwd`) from "arguments that matched
/// nothing" (empty result) before calling this.
///
/// Shared by the grep and glob executors: when grep's streamed-engine cutover
/// completes (ws43-02 seam) this expansion moves CLI-side for grep, but the
/// daemon-side copy stays for glob until glob's own cutover (ws43-03).
#[must_use]
pub fn expand_search_paths(
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
) -> Vec<PathBuf> {
    expand_search_paths_reporting(paths, include_gitignored, include_hidden).0
}

/// Like [`expand_search_paths`], but quits the moment `cancel` fires (misc 140
/// phase 2) — the cancellable flat form glob's off-thread `--count` walk uses.
#[must_use]
pub fn expand_search_paths_cancellable(
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
    cancel: &CancellationToken,
) -> Vec<PathBuf> {
    expand_search_paths_grouped_cancellable(paths, include_gitignored, include_hidden, cancel)
        .into_iter()
        .flat_map(|g| g.resolved)
        .collect()
}

/// One search-path argument's resolution: the concrete paths it contributed and
/// whether it was a glob pattern (as opposed to an existing file/directory).
///
/// [`expand_search_paths_grouped`] (grep names) and
/// [`expand_glob_patterns_grouped_cancellable`] (glob patterns) return one of
/// these per argument, in argument order, so callers that render per-argument
/// structure — glob's `no matches for pattern` report (misc 118) — know which
/// resolved paths belong to which argument and whether that argument was a
/// pattern worth reporting.
pub struct ArgResolution {
    /// The paths this argument resolved to: a single existing file/directory,
    /// or the (sorted) matches of a glob pattern. Empty when the argument
    /// contributed nothing — a zero-match pattern, a metachar-free absent, or a
    /// named gitignored directory the gate dropped.
    pub resolved: Vec<PathBuf>,
    /// True when the argument was a glob pattern expanded daemon-side. On grep's
    /// name path this is a metachar-bearing argument with no literal path on
    /// disk (false for an existing file/directory or a metachar-free absent); on
    /// glob's one-verb path *every* argument is a pattern. Only a pattern earns a
    /// `no matches for pattern` report (0 matches).
    pub is_pattern: bool,
}

/// Resolves each search-path argument independently, preserving argument order.
///
/// The per-argument primitive behind [`expand_search_paths`] and
/// [`expand_search_paths_reporting`]: each argument is classified exactly as
/// those flatten it — an existing file/directory (gitignore-gated for a named
/// directory), a metachar-bearing glob pattern expanded via
/// [`ResolvedGlob::expand`], or a metachar-free absent that contributes nothing
/// — but the results stay grouped by argument. Callers that render per-argument
/// structure (glob's zero-match report) need to know which resolved paths came
/// from which argument; callers that only want the flat set fold the groups.
#[must_use]
pub fn expand_search_paths_grouped(
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
) -> Vec<ArgResolution> {
    expand_search_paths_grouped_cancellable(
        paths,
        include_gitignored,
        include_hidden,
        &CancellationToken::new(),
    )
}

/// Like [`expand_search_paths_grouped`], but quits the moment `cancel` fires
/// (misc 140 phase 2).
///
/// The per-argument loop is checked before each argument, and every glob
/// expansion is the cancellable [`ResolvedGlob::expand_cancellable`], so a
/// single massive pattern base directory stops mid-walk once the router's
/// disconnect `select!` fires the token. A cancelled resolution returns whatever
/// arguments completed before the token fired — the caller discards the whole
/// result on cancellation, so partial content never reaches output.
#[must_use]
pub fn expand_search_paths_grouped_cancellable(
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
    cancel: &CancellationToken,
) -> Vec<ArgResolution> {
    let mut groups = Vec::with_capacity(paths.len());
    // Per-parent cache of gitignore-visible entries, so a batch of
    // shell-expanded siblings only walks their directory once.
    let mut visible: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    for path in paths {
        if cancel.is_cancelled() {
            break;
        }
        // Re-stat with a bounded retry before treating a literal path as a
        // glob — a transient stat miss (e.g. an atomic-rename write between the
        // CLI probe and here) must never silently zero a path present on disk.
        if path_exists_with_retry(path) {
            // A named existing file bypasses the gitignore (and hidden) gate —
            // the user named that exact file, so it is searched unconditionally
            // (misc 110). The gate still governs directory walks: a named
            // gitignored directory is dropped unless `include_gitignored`.
            // `is_file()` follows symlinks, so a symlink-to-file is a file and a
            // symlink-to-dir or broken symlink falls into the gated branch.
            let mut resolved = Vec::new();
            if path.is_file() || include_gitignored || !is_gitignored(path, &mut visible) {
                resolved.push(path.clone());
            }
            groups.push(ArgResolution {
                resolved,
                is_pattern: false,
            });
        } else if has_glob_metachar(&path.to_string_lossy()) {
            // Only metachar-bearing args expand as globs. A metachar-free path
            // that still does not resolve is a genuine "not found" — it is the
            // CLI's loud `path does not exist` (collected before dispatch), not
            // a glob that silently expands to an empty set.
            let mut resolved = Vec::new();
            if let Ok(glob) = ResolvedGlob::new(&path.to_string_lossy()) {
                resolved.extend(glob.expand_cancellable(
                    include_gitignored,
                    include_hidden,
                    cancel,
                ));
            }
            groups.push(ArgResolution {
                resolved,
                is_pattern: true,
            });
        } else {
            // Metachar-free absent: contributes nothing and is not a pattern
            // (the CLI reports it as `path does not exist`).
            groups.push(ArgResolution {
                resolved: Vec::new(),
                is_pattern: false,
            });
        }
    }
    groups
}

/// Resolves each `catenary glob` positional as a **pure pattern**, always —
/// no disk probe deciding semantics (VERBS ruling, the one-verb form).
///
/// The glob positional is a pattern decoded syntactically, always: every
/// argument compiles to a [`ResolvedGlob`] and expands gitignore-aware, with no
/// `is_pattern` content sniffing and no bug-13 literal-first carve-out. A
/// metachar-free argument like `src/main.rs` is a self-matching literal — the
/// glob whose only match is that exact path — so `glob src/main.rs` still
/// answers, but it goes through the same gitignore-aware walk as `src/*.rs`
/// (uniform gitignore: no metachar-free bypass; `--include-gitignored` is the
/// one lever). A directory pattern like `src` self-matches the directory entry,
/// which the listing then descends into.
///
/// Every group is `is_pattern: true` — the whole point of the one-verb form is
/// that there is no name/pattern branch. A zero-match argument is reported
/// loudly per-argument by the CLI (misc 118) with a raw-string gitignore/hidden
/// disclosure (VERBS streams ruling); nothing is ever silently classified as a
/// "path does not exist" the way a grep name operand is.
///
/// Callers absolutize each relative positional against the request `cwd` before
/// dispatch (`GlobRequest::to_params`), so `expand_cancellable` sees the
/// absolute form it needs.
#[must_use]
pub fn expand_glob_patterns_grouped_cancellable(
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
    cancel: &CancellationToken,
) -> Vec<ArgResolution> {
    let mut groups = Vec::with_capacity(paths.len());
    for path in paths {
        if cancel.is_cancelled() {
            break;
        }
        let mut resolved = Vec::new();
        if let Ok(glob) = ResolvedGlob::new(&path.to_string_lossy()) {
            resolved.extend(glob.expand_cancellable(include_gitignored, include_hidden, cancel));
        }
        groups.push(ArgResolution {
            resolved,
            is_pattern: true,
        });
    }
    groups
}

/// Like [`expand_search_paths`], additionally reporting which **glob-pattern**
/// arguments expanded to zero matches.
///
/// The second tuple element holds the indices (into `paths`) of
/// metachar-bearing arguments that resolved to no path on disk — a pattern that
/// matched nothing (or an unparseable glob). `catenary glob` surfaces these as
/// a loud per-argument `no matches for pattern: <pattern>` report (misc 118),
/// mirroring the CLI's `path does not exist` for metachar-free absents: without
/// it, a pattern passed alongside other arguments expands silently against
/// `cwd` and contributes nothing. A **metachar-free** absent is *not* reported
/// here — that is the CLI's `path does not exist`, collected before dispatch —
/// nor is an existing path a directory gitignore gate drops (naming it was not
/// a pattern).
#[must_use]
pub fn expand_search_paths_reporting(
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
) -> (Vec<PathBuf>, Vec<usize>) {
    let mut resolved = Vec::new();
    let mut no_match = Vec::new();
    for (i, group) in expand_search_paths_grouped(paths, include_gitignored, include_hidden)
        .into_iter()
        .enumerate()
    {
        // A pattern that contributed no path — expanded to nothing or failed to
        // compile — is recorded so the caller can report it loudly (misc 118)
        // instead of letting it vanish.
        if group.is_pattern && group.resolved.is_empty() {
            no_match.push(i);
        }
        resolved.extend(group.resolved);
    }
    (resolved, no_match)
}

/// Number of `symlink_metadata` attempts before treating a miss as genuine.
///
/// A transient stat miss races a sub-millisecond atomic-rename window
/// (write temp + `rename`); a few tight retries (no sleep) close the
/// in-workflow case without masking a path that is genuinely absent.
const STAT_RETRY_ATTEMPTS: u32 = 3;

/// Whether `path` resolves on disk, retrying a transient `symlink_metadata`
/// miss a bounded number of times.
///
/// Existence is probed via `symlink_metadata` so a broken symlink still counts
/// as present (matching the literal-first contract). The retry never sleeps —
/// the rename window is sub-millisecond — so a present path that lost a single
/// stat race is kept rather than silently treated as a missing glob.
fn path_exists_with_retry(path: &Path) -> bool {
    path_exists_with_retry_with(path, STAT_RETRY_ATTEMPTS, |p| p.symlink_metadata().is_ok())
}

/// Retry loop body for [`path_exists_with_retry`], with the per-attempt
/// existence probe injected.
///
/// The production helper calls this with the real `symlink_metadata` probe and
/// [`STAT_RETRY_ATTEMPTS`]; tests inject a stateful probe (e.g. miss on attempt
/// 1, hit thereafter) to prove the loop actually retries — a regression to a
/// single attempt would no longer recover a transient miss.
fn path_exists_with_retry_with(path: &Path, attempts: u32, probe: impl Fn(&Path) -> bool) -> bool {
    for attempt in 0..attempts {
        if probe(path) {
            return true;
        }
        // Yield between attempts (not after the last) so the scheduler can advance
        // the racing writer past its sub-µs atomic-rename window before the
        // re-stat. Cheap and `.await`-free (this is a sync helper). (walk-3)
        if attempt + 1 < attempts {
            std::thread::yield_now();
        }
    }
    false
}

/// Whether `s` contains a shell glob metacharacter (`* ? [ {`).
///
/// Mirrors the CLI's `contains_glob_metachar` classifier so a metachar-free
/// argument is treated as a literal path (and reported missing if absent)
/// rather than compiled into a glob that silently expands to nothing.
fn has_glob_metachar(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// Whether `path` is excluded by `.gitignore`, repo-scoped like ripgrep.
///
/// Outside a git repository nothing is gitignored, so the directory walk is
/// skipped entirely (cheap `.git` probe up the tree). Inside a repo, a
/// depth-1 walk of the parent applies the full ignore hierarchy; `path` is
/// gitignored iff it is absent from the visible set. `cache` memoizes that
/// set per parent directory.
fn is_gitignored(path: &Path, cache: &mut HashMap<PathBuf, HashSet<PathBuf>>) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if !in_git_repo(parent) {
        return false;
    }
    let entries = cache
        .entry(parent.to_path_buf())
        .or_insert_with(|| visible_entries(parent));
    !entries.contains(path)
}

/// Walks up from `dir` (inclusive) looking for a `.git` entry (a directory
/// for a normal checkout, a file for worktrees/submodules).
fn in_git_repo(dir: &Path) -> bool {
    let mut current = Some(dir);
    while let Some(d) = current {
        if d.join(".git").exists() {
            return true;
        }
        current = d.parent();
    }
    false
}

/// The gitignore-visible entries directly under `dir` (depth-1 walk).
///
/// Hidden filtering is left off — an explicitly named hidden file should not
/// be dropped here; only `.gitignore` governs this filter.
fn visible_entries(dir: &Path) -> HashSet<PathBuf> {
    WalkBuilder::new(dir)
        .max_depth(Some(1))
        .git_ignore(true)
        .hidden(false)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .collect()
}

/// Shared application container for tool servers and cross-tool infrastructure.
///
/// Creates and owns all internal servers and shared dependencies.
/// [`super::hook_router::HookRouter`] holds an `Arc<Session>` and
/// handles hook dispatch. CLI tool commands access grep/glob through
/// the IPC socket.
pub struct Session {
    /// Session-wide configuration (shared with `LspClientManager`).
    pub config: Arc<Config>,
    /// Grep tool server.
    pub grep: GrepServer,
    /// Glob tool server.
    pub glob: GlobServer,
    /// Diagnostics pipeline for `PostToolUse` hook requests.
    pub diagnostics: Arc<DiagnosticsServer>,
    /// In-memory editing state (`start_editing`/`done_editing` lifecycle).
    pub editing: EditingManager,
    /// Cross-session per-root editing guardrail (daemon mode only).
    ///
    /// `None` in single-session mode. When present, `start_editing`
    /// checks this guardrail before entering editing mode, and
    /// `done_editing` / session cleanup release all held locks.
    pub editing_guardrail: Option<Arc<EditingGuardrail>>,
    /// LSP client manager (also owns document manager).
    pub(super) client_manager: Arc<LspClientManager>,
    /// File classification and root resolution.
    fs_manager: Arc<FilesystemManager>,
    /// Path validation for LSP-aware operations.
    path_validator: Arc<RwLock<PathValidator>>,
    /// Multi-sink tracing dispatcher.
    pub logging: LoggingServer,
    /// Symbol index populated from `documentSymbol` responses (shared with grep).
    pub symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    /// Catenary instance ID (unique per process invocation).
    pub instance_id: Arc<str>,
    /// Tokio runtime handle for blocking dispatch.
    pub runtime: Handle,
    /// JSONL firehose sink, owned by the primary daemon session so clean
    /// shutdown can flush + join its writer thread. `None` for per-connection
    /// sessions, which share the already-activated `LoggingServer`.
    jsonl_sink: Option<Arc<JsonlSink>>,
    /// Daemon-owned live-state snapshot writer, shared from the primary
    /// session (`None` outside daemon mode). Action boundaries
    /// ([`Self::set_last_action`]) mark it dirty so the session board reflects
    /// the change; the writer pulls per-session `status` / `last_action` at
    /// flush time (observability ticket 05).
    pub(crate) snapshot: Option<Arc<crate::state_snapshot::SnapshotWriter>>,
    /// The session's most recent attributable action, surfaced on the snapshot
    /// session board. Set at edit / diagnostics boundaries.
    last_action: std::sync::Mutex<Option<crate::state_snapshot::LastAction>>,
    /// When the daemon last saw a hook dispatch from this session (ISO 8601),
    /// surfaced on the snapshot session board. Bumped on **every**
    /// `get_or_create_router` call — i.e. every non-catenary tool the
    /// `PreToolUse` hook forwards (`Read`, `Edit`, `Bash`, …) — so it advances
    /// far more often than `last_action`. It is the recency / liveness signal
    /// the board has no death event for (ticket 05a).
    last_seen: std::sync::Mutex<String>,
    /// `true` while a `catenary diagnostics` run is in flight for this session
    /// — drives the board's `diagnostics` status (the editing accumulator has
    /// already drained by the time the run starts).
    diagnostics_in_flight: std::sync::atomic::AtomicBool,
}

/// A daemon-less `grep`/`glob` pair, backed by an empty, never-spawned
/// [`LspClientManager`] (bug 80, leg 4).
///
/// Built by [`Session::daemon_less_search`] so the CLI can run the daemon's own
/// search pipeline in-process when the daemon is down, producing output
/// byte-identical to a daemon-served answer with no language-server coverage.
pub struct DaemonlessSearch {
    /// The grep server, LSP-manager-empty.
    pub grep: GrepServer,
    /// The glob server, LSP-manager-empty.
    pub glob: GlobServer,
}

impl DaemonlessSearch {
    /// Builds a daemon-less search pair — the `grep` and `glob` servers wired to
    /// an empty, never-spawned [`LspClientManager`] — for the CLI to run
    /// in-process when the daemon is down (bug 80, leg 4).
    ///
    /// Catenary is one binary: the search pipeline (walk, gitignore semantics,
    /// pattern compilation, exclude handling, output rendering) is library code.
    /// This constructs the *exact same* [`GrepServer`]/[`GlobServer`] the daemon
    /// serves, but with no LSP manager backing — so `execute` takes the
    /// uncovered-file rendering path the daemon already uses for a tree no
    /// language server covers, and the stdout is byte-identical to a
    /// daemon-served answer with no coverage.
    ///
    /// Unlike [`Session::new`], this performs **no** logging activation, opens
    /// **no** JSONL firehose sink, and mirrors **no** snapshot — a throwaway CLI
    /// process must not write telemetry or spawn servers. There are no roots: the
    /// CLI resolves paths against `cwd`, and an empty root set means every hit is
    /// uncovered — the honest daemon-less mode.
    #[must_use]
    pub fn from_config(config: Config) -> Self {
        let config = Arc::new(config);
        let logging = LoggingServer::new();

        let classification = super::filesystem_manager::ClassificationTables::from_config(&config);
        let fs_manager = Arc::new(FilesystemManager::with_classification(classification));

        // No symbol index: without an LSP manager nothing populates it, and a
        // never-populated index yields no `#scope` anchors and no outlines —
        // exactly the uncovered render. Skipping it keeps the CLI process lean.
        let symbol_index = None;

        let glob_config = config
            .tools
            .as_ref()
            .map_or_else(crate::config::GlobConfig::default, |t| t.glob.clone());
        let outline_suppress = compile_outline_suppress(&glob_config.outline_suppress);

        let client_manager = Arc::new(LspClientManager::new(config, logging, fs_manager.clone()));

        Self {
            grep: GrepServer {
                client_manager: client_manager.clone(),
                fs_manager: fs_manager.clone(),
                symbol_index: symbol_index.clone(),
            },
            glob: GlobServer {
                client_manager,
                fs_manager,
                symbol_index,
                outline_suppress,
            },
        }
    }
}

/// Compiles glob-outline-suppression patterns into matchers.
///
/// A basename pattern (no `/`) is prefixed with `**/` for depth-independent
/// matching; a path pattern is used verbatim. Uncompilable patterns are
/// dropped. Shared by [`Session::new`], [`Session::new_for_daemon`], and
/// [`Session::daemon_less_search`] so the three constructions never drift.
fn compile_outline_suppress(patterns: &[String]) -> Vec<globset::GlobMatcher> {
    patterns
        .iter()
        .filter_map(|pat| {
            let effective = if pat.contains('/') {
                pat.clone()
            } else {
                format!("**/{pat}")
            };
            globset::Glob::new(&effective)
                .ok()
                .map(|g| g.compile_matcher())
        })
        .collect()
}

impl Session {
    /// Creates a new `Session`, constructing all internal dependencies.
    ///
    /// Constructs the logging sinks and activates the `LoggingServer`,
    /// draining any bootstrap-buffered events. After this call, all
    /// `tracing` events flow through the logging pipeline.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "session wiring threads shared daemon deps, the JSONL sink, and the snapshot sink"
    )]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        logging: LoggingServer,
        instance_id: Arc<str>,
        runtime: Handle,
        snapshot: Option<Arc<crate::state_snapshot::SnapshotWriter>>,
    ) -> Self {
        let config = Arc::new(config);

        // JSONL firehose sink (replaces MessageDbSink); owned for flush-on-shutdown.
        // The reap policy bounds on-write growth (rotation + per-tool budget,
        // ticket 01).
        let jsonl_sink = JsonlSink::with_policy(
            &crate::paths::cache_dir(),
            instance_id.clone(),
            config.reap_policy(),
        );
        let desktop_enabled = config
            .notifications
            .as_ref()
            .and_then(|n| n.desktop)
            .unwrap_or(true);
        let desktop_sink = crate::notify::DesktopNotificationSink::with_enabled(desktop_enabled);

        // Activate — drains bootstrap buffer, enables direct dispatch. The
        // snapshot writer (daemon mode) joins as an alert-ring sink. The
        // user-facing notification queue retired (tui-rework 04): warns now
        // persist on the TUI health surface, errors ride the desktop sink, and
        // everything stays queryable in the JSONL firehose.
        let mut sinks: Vec<Arc<dyn crate::logging::Sink>> = vec![jsonl_sink.clone(), desktop_sink];
        if let Some(writer) = &snapshot {
            sinks.push(writer.clone());
        }
        logging.activate(sinks);

        let classification = super::filesystem_manager::ClassificationTables::from_config(&config);
        let fs_manager = Arc::new(FilesystemManager::with_classification(classification));
        // Install config-complete roots: each `Root` loads its `.catenary.toml`
        // at birth, so the per-root toggle gate (`is_lsp_disabled` /
        // `is_diag_disabled`) sees the config the moment the root is resolvable —
        // no separate prime step, no spawn race (ticket 00a).
        fs_manager.set_roots_rich(
            roots
                .iter()
                .map(|p| Arc::new(Root::load(p.clone())))
                .collect(),
        );

        // Build symbol index (in-memory, populated lazily from documentSymbol).
        let symbol_index = SymbolIndex::new()
            .map(|idx| Arc::new(std::sync::Mutex::new(idx)))
            .map_err(|e| tracing::info!("symbol index unavailable: {e}"))
            .ok();

        let glob_config = config
            .tools
            .as_ref()
            .map_or_else(crate::config::GlobConfig::default, |t| t.glob.clone());

        let path_validator = Arc::new(RwLock::new(PathValidator::new(roots)));
        let mut client_manager =
            LspClientManager::new(config.clone(), logging.clone(), fs_manager.clone());
        if let Some(writer) = &snapshot {
            client_manager.set_snapshot(writer.clone());
        }
        let client_manager = Arc::new(client_manager);

        let diagnostics = Arc::new(DiagnosticsServer::new(
            client_manager.clone(),
            path_validator.clone(),
            fs_manager.clone(),
            symbol_index.clone(),
        ));

        let grep = GrepServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            symbol_index: symbol_index.clone(),
        };
        let outline_suppress = compile_outline_suppress(&glob_config.outline_suppress);
        let glob = GlobServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            symbol_index: symbol_index.clone(),
            outline_suppress,
        };
        Self {
            config,
            grep,
            glob,
            diagnostics,
            editing: EditingManager::new(),
            editing_guardrail: None,
            client_manager,
            fs_manager,
            path_validator,
            logging,
            symbol_index,
            instance_id,
            runtime,
            jsonl_sink: Some(jsonl_sink),
            snapshot,
            last_action: std::sync::Mutex::new(None),
            last_seen: std::sync::Mutex::new(crate::state_snapshot::now_iso()),
            diagnostics_in_flight: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Creates a per-session `Session` for daemon mode.
    ///
    /// Shares heavy resources (`LspClientManager`, `FilesystemManager`,
    /// `SymbolIndex`, config, logging) with the daemon's primary session.
    /// Creates fresh per-session state: editing manager and editing
    /// guardrail.
    #[must_use]
    pub fn new_for_daemon(
        primary: &Self,
        session_id: Arc<str>,
        editing_guardrail: Option<Arc<EditingGuardrail>>,
    ) -> Self {
        let glob_config = primary
            .config
            .tools
            .as_ref()
            .map_or_else(crate::config::GlobConfig::default, |t| t.glob.clone());

        let outline_suppress = compile_outline_suppress(&glob_config.outline_suppress);

        Self {
            config: primary.config.clone(),
            grep: GrepServer {
                client_manager: primary.client_manager.clone(),
                fs_manager: primary.fs_manager.clone(),
                symbol_index: primary.symbol_index.clone(),
            },
            glob: GlobServer {
                client_manager: primary.client_manager.clone(),
                fs_manager: primary.fs_manager.clone(),
                symbol_index: primary.symbol_index.clone(),
                outline_suppress,
            },
            diagnostics: primary.diagnostics.clone(),
            editing: EditingManager::new(),
            editing_guardrail,
            client_manager: primary.client_manager.clone(),
            fs_manager: primary.fs_manager.clone(),
            path_validator: primary.path_validator.clone(),
            logging: primary.logging.clone(),
            symbol_index: primary.symbol_index.clone(),
            instance_id: session_id,
            runtime: primary.runtime.clone(),
            jsonl_sink: None,
            snapshot: primary.snapshot.clone(),
            last_action: std::sync::Mutex::new(None),
            last_seen: std::sync::Mutex::new(crate::state_snapshot::now_iso()),
            diagnostics_in_flight: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Records the session's most recent action and marks the snapshot dirty.
    ///
    /// Surfaced on the snapshot session board's `last_action` field
    /// (observability ticket 05). Called at edit and diagnostics boundaries.
    /// The snapshot lock is taken only after the `last_action`
    /// guard is dropped, so this never inverts lock order against the flush
    /// path (which reads `last_action` while pulling the board).
    pub fn set_last_action(&self, summary: impl Into<String>) {
        let action = crate::state_snapshot::LastAction {
            summary: summary.into(),
            at: crate::state_snapshot::now_iso(),
        };
        {
            let mut guard = self
                .last_action
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(action);
        }
        self.touch_snapshot();
    }

    /// Returns the session's most recent action, if any.
    #[must_use]
    pub fn last_action(&self) -> Option<crate::state_snapshot::LastAction> {
        self.last_action
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Bumps `last_seen` to now and marks the snapshot dirty.
    ///
    /// Called on **every** session-bound hook dispatch (the
    /// `get_or_create_router` chokepoint), so it tracks recency — the only
    /// uniform liveness signal a hook session has, since the hook side carries
    /// no authoritative death event (ticket 05a). Distinct from
    /// [`Self::set_last_action`], which moves only on edit / diagnostics.
    /// Like that method, the snapshot lock is taken only after the `last_seen`
    /// guard is dropped, so it never inverts lock order against the flush path.
    pub fn touch_last_seen(&self) {
        {
            let mut guard = self
                .last_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = crate::state_snapshot::now_iso();
        }
        self.touch_snapshot();
    }

    /// Returns the session's most recent hook-dispatch time (ISO 8601).
    #[must_use]
    pub fn last_seen(&self) -> String {
        self.last_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Closes every held-open document a ROOT owns, across all server
    /// connections — the batch-end leg of the held-open lifecycle
    /// (diagnostics-debt 01, re-keyed to the root in root-ownership stage 3).
    ///
    /// Documents a diagnose round opens are tagged with their root
    /// ([`crate::lsp::manager::LspClientManager::open_document_on`]), so root
    /// retirement (worktree removal) closes exactly that root's held-open
    /// documents — no identity below the hook. The owner string is the root's
    /// display path, the same spelling the serve path tags with.
    pub async fn close_root_docs(&self, root: &std::path::Path) {
        let owner = root.to_string_lossy();
        self.client_manager.close_agent_docs(&owner).await;
    }

    /// Sets whether a `catenary diagnostics` run is in flight and marks the
    /// snapshot dirty so the board's status reflects the transition promptly.
    pub fn set_diagnostics_in_flight(&self, in_flight: bool) {
        self.diagnostics_in_flight
            .store(in_flight, std::sync::atomic::Ordering::Release);
        self.touch_snapshot();
    }

    /// Derives the session's board status from live editing state, keyed to the
    /// durable LEDGER for the armed/paid axis (bug 116).
    ///
    /// The editing debt gate is the truth source (tui-rework 14, item 1),
    /// evaluated at snapshot-build time with no transition tracking:
    ///
    /// - `diagnostics` while a `catenary diagnostics` run is in flight;
    /// - `editing` when the gate is **armed** — any of the session's roots holds
    ///   unpaid debt on its ledger ([`crate::lock::has_debt`]);
    /// - `working` when the gate is **paid** but an editing accumulator is still
    ///   held — the ledger is clear, yet the session is mid-edit;
    /// - `idle` when no accumulator is active (the session did `done_editing`).
    ///
    /// The armed/paid axis reads the ledger, not the retired in-memory `delivered`
    /// flags (root-ownership stage 3 left nothing in production to pay them, so the
    /// board hung at `editing` from the first edit until a Stop — bug 116). The
    /// accumulator presence ([`is_active`](crate::bridge::editing_manager::EditingManager::is_active))
    /// still separates `working`/`idle`: it is set from `start_editing` to
    /// `done_editing`, indifferent to daemon churn only for its own lifetime.
    #[must_use]
    pub fn status(&self) -> crate::state_snapshot::SessionStatus {
        self.status_in(&crate::lock::locks_dir())
    }

    /// The ledger-base-injectable core of [`status`](Self::status) (bug 116).
    ///
    /// `locks_base` lets a unit test point the debt read at a tempdir ledger
    /// without mutating the process environment (`std::env::set_var` is forbidden
    /// under Rust 2024). Production calls [`status`](Self::status), which resolves
    /// the base through [`crate::lock::locks_dir`].
    #[must_use]
    pub fn status_in(&self, locks_base: &Path) -> crate::state_snapshot::SessionStatus {
        use crate::state_snapshot::SessionStatus;
        if self
            .diagnostics_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            SessionStatus::Diagnostics
        } else if self.any_root_has_debt(locks_base) {
            SessionStatus::Editing
        } else if self.editing.is_active() {
            SessionStatus::Working
        } else {
            SessionStatus::Idle
        }
    }

    /// Whether any of the session's roots carries unpaid debt on its ledger under
    /// `locks_base` (bug 116) — the board's "is the gate armed?" question,
    /// answered by the durable per-root touch-tree.
    ///
    /// Each root is canonicalized inside [`crate::lock::has_debt_in`]'s resolution
    /// (the encoding is keyed on the canonical path); the session's roots arrive
    /// canonicalized (the tracker canonicalizes every root), so a symlinked-prefix
    /// alias reads the same lock dir the edit seam booked under.
    fn any_root_has_debt(&self, locks_base: &Path) -> bool {
        self.roots()
            .iter()
            .any(|root| crate::lock::has_debt_in(locks_base, root))
    }

    /// Derives a subagent's board status from its own per-`(session, agent)`
    /// editing batch, keyed to the durable LEDGER for the armed/paid axis
    /// (tui-rework 14, item 3; re-keyed bug 116).
    #[must_use]
    pub fn subagent_status(&self, agent_id: &str) -> crate::state_snapshot::SessionStatus {
        self.subagent_status_in(agent_id, &crate::lock::locks_dir())
    }

    /// The ledger-base-injectable core of [`subagent_status`](Self::subagent_status)
    /// (bug 116).
    ///
    /// Same gate axis as [`status`](Self::status), but the candidate set is scoped
    /// to one agent's batch: `editing` when any of the subagent's edited files is
    /// still DUE on its root's ledger
    /// ([`due_candidates_in`](crate::lock::due_candidates_in)), `working` when it
    /// holds a batch but none is due, `idle` when it has no accumulator. A subagent
    /// never runs its own `catenary diagnostics` pass through the parent's in-flight
    /// flag, so `diagnostics` is not a subagent status. `locks_base` is injected for
    /// the same tempdir-testability reason as [`status_in`](Self::status_in).
    #[must_use]
    pub fn subagent_status_in(
        &self,
        agent_id: &str,
        locks_base: &Path,
    ) -> crate::state_snapshot::SessionStatus {
        use crate::state_snapshot::SessionStatus;
        let session_id = Some(&*self.instance_id);
        if !self.editing.has_files(session_id, agent_id) {
            return SessionStatus::Idle;
        }
        let candidates = self.editing.files(session_id, agent_id);
        if crate::lock::due_candidates_in(locks_base, &candidates).is_empty() {
            SessionStatus::Working
        } else {
            SessionStatus::Editing
        }
    }

    /// Marks the snapshot dirty (coalesced flush). No-op outside daemon mode.
    pub fn touch_snapshot(&self) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.touch();
        }
    }

    /// Records a curated milestone on the snapshot's activity ring. No-op
    /// outside daemon mode (observability ticket 08). Used by session /
    /// editing / diagnostics boundaries to promote a significant event into the
    /// dashboard's live glimpse without tailing the firehose.
    pub fn record_milestone(
        &self,
        kind: crate::state_snapshot::MilestoneKind,
        summary: impl Into<String>,
        scope: Option<String>,
    ) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_milestone(kind, summary, scope);
        }
    }

    /// Renders a path for a `last_action` summary: relative to its workspace
    /// root when resolvable (e.g. `src/db.rs`), else the bare file name, else
    /// the full path.
    #[must_use]
    pub fn display_path(&self, path: &Path) -> String {
        if let Some(root) = self.resolve_root(path)
            && let Ok(rel) = path.strip_prefix(&root)
        {
            return rel.display().to_string();
        }
        path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// Records that tracked-session activity touched `path`, making its
    /// configured language **activity-live** for the health dashboard
    /// (tui-rework 09, item 5).
    ///
    /// Classifies `path` against the configured language set (the same
    /// filename/extension-then-shebang rule the workspace scan uses) and, on a
    /// match, records the language, its enclosing tracked root, and the
    /// root-relative file on the snapshot's activity ledger — the gate that keeps
    /// a dormant fixture directory no one opened quiet, and the provenance the
    /// TUI/doctor render under a routed-broken or suggestion finding.
    /// Independent of diagnostics coverage: a language whose server is not even
    /// installed still becomes live (that is precisely the install-suggestion
    /// case). No-op outside daemon mode, for an unconfigured language, or a path
    /// under no tracked root.
    pub fn record_activity_touch(&self, path: &Path) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(language) = self.configured_language(path) else {
            return;
        };
        let Some(root) = self.resolve_root(path) else {
            return;
        };
        let file = path
            .strip_prefix(&root)
            .map_or_else(|_| self.display_path(path), |rel| rel.display().to_string());
        snapshot.record_activity(&language, &root.display().to_string(), &file);
    }

    /// Retires a root from the snapshot's language-activity ledger (bug 93):
    /// every `(language, root)` provenance bucket for `root` leaves the ledger.
    ///
    /// The provenance counterpart to a per-root server teardown — called when a
    /// worktree is landed/removed so the doctor and TUI stop rendering `routed
    /// by … in <removed root>` against a path that can no longer route anything.
    /// No-op outside daemon mode.
    pub fn forget_root_activity(&self, root: &Path) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.forget_root(&root.display().to_string());
        }
    }

    /// Classify `path`'s language, restricted to the configured language set —
    /// filename/extension then shebang, with a raw-extension fallback for custom
    /// languages, mirroring `detect_workspace_languages`'s per-file rule.
    fn configured_language(&self, path: &Path) -> Option<String> {
        if let Some(lang) = self.fs_manager.language_id(path)
            && self.config.language.contains_key(lang.as_str())
        {
            return Some(lang);
        }
        let ext = path.extension().and_then(|e| e.to_str())?;
        self.config
            .language
            .contains_key(ext)
            .then(|| ext.to_string())
    }

    /// Builds the merged command filter from user config + all project configs.
    ///
    /// Returns `None` when no `[commands]` section is configured. The merged
    /// result reflects the current workspace roots and project configs —
    /// adding a root expands the allow surface.
    #[must_use]
    pub fn merged_commands(&self) -> Option<crate::config::ResolvedCommands> {
        let base = self.config.resolved_commands.as_ref()?;
        let roots = self.client_manager.roots();
        let project_commands = self.client_manager.project_commands();
        Some(base.merge_project_commands(&roots, &project_commands))
    }

    /// Returns `true` if the path is within any known workspace root.
    ///
    /// Simple prefix check against known roots — no canonicalization or
    /// symlink resolution. Used for hook scope gating where approximate
    /// checking is sufficient.
    #[must_use]
    pub fn is_within_roots(&self, path: &Path) -> bool {
        self.fs_manager.resolve_root(path).is_some()
    }

    /// Returns the workspace root containing the given path, if any.
    ///
    /// Longest-prefix match against known roots. Used by the editing
    /// guardrail to lock the specific root being edited rather than
    /// all session roots.
    #[must_use]
    pub fn resolve_root(&self, path: &Path) -> Option<PathBuf> {
        self.fs_manager.resolve_root(path)
    }

    /// Returns `true` if the path has known LSP coverage for diagnostics.
    ///
    /// Both tiers require the file's language to be actually served: an
    /// in-root file (tiers 1–2) is covered when a server is *configured* for
    /// its language ([`has_configured_server`], independent of instance
    /// state — a cold per-root instance of a warm language still counts,
    /// granularity Decision 3); an out-of-root file (tier 3) is covered when
    /// its language has a server with a positive single-file cache entry
    /// ([`has_single_file_coverage`]). Files whose language is unknown, has no
    /// configured server (e.g. `.txt`, logs, data/scratch files), or has only
    /// an uncached / negative-cached single-file server return `false` — the
    /// editing gate should not impose friction on edits it cannot diagnose.
    ///
    /// A root with `disable_lsp` set (ticket 00) runs no language server, so its
    /// files have no LSP coverage regardless of configured servers.
    ///
    /// [`has_configured_server`]: crate::lsp::LspClientManager::has_configured_server
    /// [`has_single_file_coverage`]: crate::lsp::LspClientManager::has_single_file_coverage
    #[must_use]
    pub fn has_lsp_coverage(&self, path: &Path) -> bool {
        let lang = self.fs_manager.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
        if let Some(root) = self.fs_manager.resolve_root(path) {
            if self.client_manager.is_lsp_disabled(&root) {
                return false;
            }
            return lang.is_some_and(|id| self.client_manager.has_configured_server(&root, &id));
        }
        lang.is_some_and(|id| self.client_manager.has_single_file_coverage(&id))
    }

    /// Whether *any* diagnostic feeder covers this file.
    ///
    /// `has_coverage = has_lsp_coverage || has_lint_coverage` (workstream 34
    /// ticket 00). The editing-boundary gate tracks/gates a file iff some
    /// feeder covers it. [`has_lint_coverage`](Self::has_lint_coverage) is
    /// stubbed `false` until the linter framework lands (ticket 01), so today
    /// this equals [`has_lsp_coverage`](Self::has_lsp_coverage).
    #[must_use]
    pub fn has_coverage(&self, path: &Path) -> bool {
        self.has_lsp_coverage(path) || self.has_lint_coverage(path)
    }

    /// Whether a standalone linter covers this file (workstream 34 ticket 01).
    ///
    /// Resolves the file to its owning root and matches the root-relative path
    /// against that root's effective `[linter.rule.*]` patterns (user ∪ project),
    /// reusing `LspGlob`. Out-of-root files and `disable_lint` roots are never
    /// covered. With no `[linter.rule.*]` configured (defaults ship in ticket 03)
    /// this is `false`, so the coverage gate is unchanged until a linter is set.
    #[must_use]
    pub fn has_lint_coverage(&self, path: &Path) -> bool {
        self.client_manager.lint_covers(path)
    }

    /// Whether this file's *only* diagnostics coverage is an **unverified**
    /// (enrichment-only) server — a diagnostics server exists but is not blessed,
    /// so Catenary withholds its diagnostics (diagnostics-debt 04b).
    ///
    /// The signal that separates the "not diagnostics-covered" skip bucket (a
    /// server exists, unblessed) from the truly-uncovered bucket (no server at
    /// all). Only meaningful when [`has_coverage`](Self::has_coverage) is already
    /// `false` — a blessed server or a linter would make the file genuinely
    /// covered — so a caller checks coverage first, then this. A `disable_lsp`
    /// root runs no server, so its files are never unverified-only. Out-of-root
    /// files have no per-root config to consult and return `false`.
    #[must_use]
    pub fn has_unverified_only_coverage(&self, path: &Path) -> bool {
        let Some(root) = self.fs_manager.resolve_root(path) else {
            return false;
        };
        if self.client_manager.is_lsp_disabled(&root) {
            return false;
        }
        let lang = self.fs_manager.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
        lang.is_some_and(|id| self.client_manager.has_unverified_only_server(&root, &id))
    }

    /// The diagnostic feeders — LSP servers and standalone linters — that track
    /// `path`, sorted and deduplicated by name.
    ///
    /// A config-level projection of the editing-gate coverage predicates
    /// ([`has_lsp_coverage`](Self::has_lsp_coverage) +
    /// [`has_lint_coverage`](Self::has_lint_coverage)): every feeder here would
    /// report on the file when `catenary diagnostics` runs. The editing-gate
    /// message groups its outstanding files by these names so the agent sees
    /// which tool checks each. A file the gate tracks
    /// ([`covered_for_diagnostics`](Self::covered_for_diagnostics)) always
    /// yields at least one feeder.
    #[must_use]
    pub fn diagnostic_feeders(&self, path: &Path) -> Vec<String> {
        self.client_manager.diagnostic_feeder_names(path)
    }

    /// Whether the diagnostics surface is suppressed for the file's root
    /// (`disable_diag`, ticket 00).
    ///
    /// Out-of-root files have no owning root to consult and are never disabled.
    #[must_use]
    pub fn diag_disabled(&self, path: &Path) -> bool {
        self.fs_manager
            .resolve_root(path)
            .is_some_and(|root| self.client_manager.is_diag_disabled(&root))
    }

    /// The editing-boundary gate predicate (ticket 00).
    ///
    /// Track/gate a file for batched diagnostics iff some feeder covers it
    /// ([`has_coverage`](Self::has_coverage)) AND its root has not suppressed
    /// the diagnostics surface (`disable_diag`). `disable_diag` keeps LSP
    /// navigation (grep/glob) but turns the gate + output off, so a covered
    /// file in such a root flows free.
    #[must_use]
    pub fn covered_for_diagnostics(&self, path: &Path) -> bool {
        self.has_coverage(path) && !self.diag_disabled(path)
    }

    /// Returns the shared `LspClientManager`.
    ///
    /// Used by the daemon's `SessionManager` to wire MCP lifecycle
    /// callbacks (`on_roots_changed`) directly to the shared
    /// infrastructure without routing through a `Session`.
    #[must_use]
    pub(crate) const fn lsp_client_manager(&self) -> &Arc<LspClientManager> {
        &self.client_manager
    }

    /// Returns the current workspace roots.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.client_manager.roots()
    }

    /// Spawns LSP servers for languages detected in the workspace.
    pub async fn spawn_all(&self) {
        self.client_manager.spawn_all().await;
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Updates path validation, notifies LSP servers of folder changes,
    /// and spawns servers for any newly detected languages.
    ///
    /// `roots` are config-complete [`Root`]s (loaded at birth by the daemon's
    /// root tracker, ticket 00a). The manager consumes the rich roots (config +
    /// classification); the path validator gets the path-only view.
    ///
    /// # Errors
    ///
    /// Returns an error if root synchronization fails.
    pub async fn sync_roots(&self, roots: Vec<Arc<Root>>) -> Result<()> {
        self.sync_roots_inner(roots, true).await
    }

    /// Like [`sync_roots`](Self::sync_roots) but **without** the eager `spawn_all`
    /// pre-warm — the boot-restore path for persisted pins (misc 175).
    ///
    /// Registers the roots (so a first-touch tool call resolves them) and runs the
    /// manager's `spawn_for_added_roots` leg, which is a no-op on a fresh daemon
    /// (no language is active elsewhere yet). Skipping the pre-warm keeps the
    /// zero-cost-restore promise: a restored pin is a tracker entry and a
    /// roots-board line until first use, when the ordinary lazy first-touch spawn
    /// pays. The runtime `catenary pin` keeps its warm-language pre-warm via
    /// [`sync_roots`](Self::sync_roots); only boot restore uses this leg.
    ///
    /// # Errors
    ///
    /// Returns an error if root synchronization fails.
    pub async fn sync_roots_no_prewarm(&self, roots: Vec<Arc<Root>>) -> Result<()> {
        self.sync_roots_inner(roots, false).await
    }

    /// Shared root-sync body. `prewarm` gates the fire-and-forget `spawn_all`:
    /// on for the ordinary path (a pin/MCP-sync pre-warms), off for boot restore.
    async fn sync_roots_inner(&self, roots: Vec<Arc<Root>>, prewarm: bool) -> Result<()> {
        // Path-only view for the validator (a path-only consumer).
        let paths: Vec<PathBuf> = roots.iter().map(|r| r.path().to_path_buf()).collect();

        // sync_roots updates FilesystemManager roots first (before any
        // async work), then reacts to the diff.
        let removed = self.client_manager.sync_roots(roots).await?;
        self.path_validator.write().await.update_roots(paths);

        // Evict the per-root `SymbolIndex` entries for every removed root —
        // the manager owns no handle to the index, so this is the only layer
        // where both the removed set and the index are visible. Without it the
        // daemon-lived cache outlives a root's tracked lifetime and serves
        // enrichment for a path `catenary roots ls` reports as untracked
        // (bug #36). The per-root baseline / `root_generations` teardown for the
        // same removed roots already happens inside `LspClientManager`.
        if let Some(index) = &self.symbol_index {
            let idx = index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for root in &removed {
                idx.evict_root(root);
            }
        }

        // Fire-and-forget: spawn_all is pre-warming, not a gate.
        // Tool calls that need a server will trigger spawning on demand.
        // Boot restore (misc 175) skips it so a restored pin spawns nothing.
        if prewarm {
            let cm = self.client_manager.clone();
            tokio::spawn(async move { cm.spawn_all().await });
        }
        Ok(())
    }

    /// Shuts down all active LSP servers gracefully.
    pub async fn shutdown(&self) {
        self.client_manager.shutdown_all().await;
    }

    /// Flush the JSONL firehose and stop its writer thread on clean shutdown.
    ///
    /// Drains the queued lines and joins the writer so the firehose tail lands
    /// on disk before the daemon exits. Only the primary daemon session owns the
    /// sink; per-connection sessions hold `None` and this is a no-op. Call after
    /// [`Session::shutdown`] so LSP-shutdown telemetry is captured too.
    pub fn flush_telemetry(&self) {
        if let Some(sink) = &self.jsonl_sink {
            sink.shutdown();
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    // ── expand_tilde ──────────────────────────────────────────────

    #[test]
    fn expand_tilde_home_prefix() {
        let home = std::env::var("HOME").expect("HOME must be set");
        assert_eq!(expand_tilde("~/foo/bar"), format!("{home}/foo/bar"));
    }

    #[test]
    fn expand_tilde_bare() {
        let home = std::env::var("HOME").expect("HOME must be set");
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn expand_tilde_no_op_for_absolute() {
        assert_eq!(expand_tilde("/usr/bin"), "/usr/bin");
    }

    #[test]
    fn expand_tilde_no_op_for_relative() {
        assert_eq!(expand_tilde("src/main.rs"), "src/main.rs");
    }

    // ── ExcludeSet (bug 89) ───────────────────────────────────────

    #[test]
    fn exclude_set_empty_is_noop() {
        let set = ExcludeSet::compile(&[]).expect("compile empty");
        assert!(set.is_empty());
        assert!(
            !set.is_match(Path::new("/root/a.rs"), Path::new("/root")),
            "an empty set matches nothing"
        );
    }

    #[test]
    fn exclude_set_matches_any_pattern() {
        // A repeated `--exclude-pattern` unions its patterns: a path is excluded
        // when ANY glob in the set matches it (bug 89).
        let set = ExcludeSet::compile(&["/root/*.rs".to_string(), "/root/*.txt".to_string()])
            .expect("compile");
        assert!(!set.is_empty());
        let root = Path::new("/root");
        assert!(set.is_match(Path::new("/root/a.rs"), root), "first pattern");
        assert!(
            set.is_match(Path::new("/root/b.txt"), root),
            "second pattern"
        );
        assert!(
            !set.is_match(Path::new("/root/keep.md"), root),
            "a path matching neither pattern survives"
        );
    }

    #[test]
    fn exclude_set_skips_empty_strings() {
        // An empty pattern string contributes nothing — the set stays empty.
        let set = ExcludeSet::compile(&[String::new()]).expect("compile");
        assert!(set.is_empty(), "empty-string patterns are skipped");
    }

    #[test]
    fn expand_tilde_no_op_for_mid_tilde() {
        assert_eq!(expand_tilde("foo/~/bar"), "foo/~/bar");
    }

    // ── ResolvedGlob::targets_hidden ───────────────────────────────

    #[test]
    fn targets_hidden_dotfile() {
        assert!(ResolvedGlob::targets_hidden(".gitignore"));
    }

    #[test]
    fn targets_hidden_dotdir_glob() {
        assert!(ResolvedGlob::targets_hidden(".github/*.yml"));
    }

    #[test]
    fn targets_hidden_dot_prefix_glob() {
        assert!(ResolvedGlob::targets_hidden(".git*"));
    }

    #[test]
    fn targets_hidden_nested_dotdir() {
        assert!(ResolvedGlob::targets_hidden("src/.hidden/foo.rs"));
    }

    #[test]
    fn targets_hidden_dotfile_toml() {
        assert!(ResolvedGlob::targets_hidden(".catenary.toml"));
    }

    #[test]
    fn targets_hidden_broad_doublestar() {
        assert!(!ResolvedGlob::targets_hidden("**/*.rs"));
    }

    #[test]
    fn targets_hidden_broad_src() {
        assert!(!ResolvedGlob::targets_hidden("src/**/*"));
    }

    #[test]
    fn targets_hidden_broad_star() {
        assert!(!ResolvedGlob::targets_hidden("**/*"));
    }

    #[test]
    fn targets_hidden_dotdot_is_not_hidden() {
        assert!(!ResolvedGlob::targets_hidden("../src/*.rs"));
    }

    #[test]
    fn targets_hidden_single_dot_is_not_hidden() {
        assert!(!ResolvedGlob::targets_hidden("./src/*.rs"));
    }

    // ── glob expansion ─────────────────────────────────────────────

    #[test]
    fn expand_matches_recursively_with_doublestar() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
        std::fs::write(root.join("a/b/deep.rs"), "x").expect("write");
        std::fs::write(root.join("top.rs"), "x").expect("write");
        std::fs::write(root.join("a/note.txt"), "x").expect("write");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = ResolvedGlob::new(&pattern).expect("compile glob");
        let matches = glob.expand(false, false);

        assert!(matches.contains(&root.join("a/b/deep.rs")), "{matches:?}");
        assert!(matches.contains(&root.join("top.rs")), "{matches:?}");
        assert!(
            !matches.iter().any(|p| p.ends_with("note.txt")),
            "non-matching extension excluded: {matches:?}"
        );
    }

    /// Initializes a git repo at `dir` so gitignore rules apply (gitignore is
    /// repo-scoped: outside a repo no `.gitignore` is honored).
    fn git_init(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");
    }

    #[test]
    fn expand_is_gitignore_aware() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::write(root.join("target/ignored.rs"), "x").expect("write");
        std::fs::write(root.join("kept.rs"), "x").expect("write");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = ResolvedGlob::new(&pattern).expect("compile glob");

        let matches = glob.expand(false, false);
        assert!(matches.contains(&root.join("kept.rs")), "{matches:?}");
        assert!(
            !matches.iter().any(|p| p.ends_with("ignored.rs")),
            "gitignored target/ pruned: {matches:?}"
        );

        // The escape hatch lifts the filter.
        let with_ignored = glob.expand(true, false);
        assert!(
            with_ignored.iter().any(|p| p.ends_with("ignored.rs")),
            "include_gitignored surfaces target/: {with_ignored:?}"
        );
    }

    #[test]
    fn expand_search_paths_keeps_named_gitignored_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(root.join("ignored.rs"), "x").expect("write");
        std::fs::write(root.join("kept.rs"), "x").expect("write");

        let ignored = root.join("ignored.rs");
        let kept = root.join("kept.rs");

        // A named existing gitignored FILE is searched unconditionally — naming
        // it is a direct request for that exact file, so the gitignore gate does
        // not apply even without `--include-gitignored` (misc 110, ripgrep
        // parity).
        let resolved = expand_search_paths(&[ignored.clone(), kept.clone()], false, false);
        assert_eq!(resolved, vec![ignored.clone(), kept], "{resolved:?}");

        // The escape hatch is also a no-op for an already-kept named file.
        let with_ignored = expand_search_paths(std::slice::from_ref(&ignored), true, false);
        assert_eq!(with_ignored, vec![ignored], "{with_ignored:?}");
    }

    #[test]
    fn expand_search_paths_still_gates_named_gitignored_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::write(root.join("target/ignored.rs"), "x").expect("write");

        let dir = root.join("target");

        // A named gitignored DIRECTORY is still dropped — the gate governs the
        // recursive directory walk, not a named file. `--include-gitignored`
        // remains the opt-in for directory contents (directory-walk behavior
        // unchanged, misc 110).
        let gated = expand_search_paths(std::slice::from_ref(&dir), false, false);
        assert!(gated.is_empty(), "named gitignored dir is gated: {gated:?}");

        // The escape hatch lifts the directory gate.
        let with_ignored = expand_search_paths(std::slice::from_ref(&dir), true, false);
        assert_eq!(with_ignored, vec![dir], "{with_ignored:?}");
    }

    #[test]
    fn expand_single_star_does_not_cross_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("flat.rs"), "x").expect("write");
        std::fs::write(root.join("sub/nested.rs"), "x").expect("write");

        let pattern = format!("{}/*.rs", root.display());
        let glob = ResolvedGlob::new(&pattern).expect("compile glob");
        let matches = glob.expand(false, false);

        assert!(matches.contains(&root.join("flat.rs")), "{matches:?}");
        assert!(
            !matches.contains(&root.join("sub/nested.rs")),
            "single star stays within one segment: {matches:?}"
        );
    }

    /// Bounding the expansion walk to the pattern's depth (misc 159) must not
    /// truncate a legitimate deeper match: a two-segment single-star pattern
    /// (`.../mid*/leaf*.rs`) matches at depth 2 below the base, so the walk is
    /// bounded to depth 2 — deep enough to find `midX/leafY.rs`, but not so deep
    /// it descends a matched intermediate dir's own subtree.
    #[test]
    fn expand_multi_segment_single_star_reaches_its_depth() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("mid1/deeper")).expect("mkdir");
        std::fs::write(root.join("mid1/leafA.rs"), "x").expect("write");
        std::fs::write(root.join("mid1/deeper/tooDeep.rs"), "x").expect("write");
        std::fs::create_dir_all(root.join("mid2")).expect("mkdir");
        std::fs::write(root.join("mid2/leafB.rs"), "x").expect("write");

        let pattern = format!("{}/mid*/leaf*.rs", root.display());
        let glob = ResolvedGlob::new(&pattern).expect("compile glob");
        let matches = glob.expand(false, false);

        assert!(
            matches.contains(&root.join("mid1/leafA.rs")),
            "depth-2 match found: {matches:?}"
        );
        assert!(
            matches.contains(&root.join("mid2/leafB.rs")),
            "depth-2 match in a second matched mid dir found: {matches:?}"
        );
        assert!(
            !matches.contains(&root.join("mid1/deeper/tooDeep.rs")),
            "a single-star pattern never matches past its depth: {matches:?}"
        );
    }

    #[test]
    fn expand_search_paths_keeps_existing_and_expands_patterns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("real.rs"), "x").expect("write");
        std::fs::write(root.join("other.rs"), "x").expect("write");

        let existing = root.join("real.rs");
        // An existing path passes through; a non-glob, non-existent path
        // expands to nothing (the CLI reports those as `path does not exist`).
        let resolved =
            expand_search_paths(&[existing.clone(), root.join("ghost.rs")], false, false);
        assert_eq!(resolved, vec![existing], "{resolved:?}");

        // A pattern expands to its matches.
        let expanded = expand_search_paths(&[root.join("*.rs")], false, false);
        assert!(expanded.contains(&root.join("real.rs")), "{expanded:?}");
        assert!(expanded.contains(&root.join("other.rs")), "{expanded:?}");
    }

    #[test]
    fn path_exists_with_retry_succeeds_for_present_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("present.rs");
        std::fs::write(&file, "x").expect("write");
        assert!(
            path_exists_with_retry(&file),
            "a present file resolves through the bounded retry"
        );
    }

    #[test]
    fn path_exists_with_retry_fails_for_absent_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ghost = tmp.path().join("ghost.rs");
        assert!(
            !path_exists_with_retry(&ghost),
            "a genuinely absent path stays absent after the bounded retry"
        );
    }

    #[test]
    fn path_exists_with_retry_keeps_broken_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let link = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("missing-target"), &link).expect("symlink");
        assert!(
            path_exists_with_retry(&link),
            "a broken symlink is present (symlink_metadata succeeds)"
        );
    }

    #[test]
    fn ws31_review_r2_live_retry_recovers_transient_miss() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A stateful probe that misses on call 1 and hits on every later call —
        // the deterministic transient miss→hit a real atomic-rename race would
        // produce. With the full `STAT_RETRY_ATTEMPTS` budget the loop must
        // recover; with a single attempt it must NOT — so the guard is sensitive
        // to the retry count (a regression to `attempts == 1` fails here, where a
        // terminal present/absent test would still pass).
        const {
            assert!(
                STAT_RETRY_ATTEMPTS >= 2,
                "the retry guard assumes more than one attempt"
            );
        }

        let calls = AtomicUsize::new(0);
        let probe = |_: &Path| calls.fetch_add(1, Ordering::Relaxed) >= 1;
        let path = Path::new("/does/not/matter");

        assert!(
            path_exists_with_retry_with(path, STAT_RETRY_ATTEMPTS, probe),
            "the bounded retry must recover a miss that resolves on a later attempt"
        );

        // Same probe, fresh counter, single attempt: the first call misses and
        // there is no retry, so the loop reports absent — pinning the retry-count
        // sensitivity (a `STAT_RETRY_ATTEMPTS = 1` regression would surface here).
        let calls = AtomicUsize::new(0);
        let probe = |_: &Path| calls.fetch_add(1, Ordering::Relaxed) >= 1;
        assert!(
            !path_exists_with_retry_with(path, 1, probe),
            "a single attempt cannot recover a transient miss"
        );
    }

    #[test]
    fn has_glob_metachar_matches_cli_classifier() {
        assert!(has_glob_metachar("*.rs"));
        assert!(has_glob_metachar("a?b"));
        assert!(has_glob_metachar("[abc].rs"));
        assert!(has_glob_metachar("{a,b}.rs"));
        assert!(!has_glob_metachar("plain.rs"));
        assert!(!has_glob_metachar("src/bridge/session.rs"));
    }

    #[test]
    fn expand_search_paths_metachar_free_absent_is_not_glob_expanded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("real.rs"), "x").expect("write");

        // A metachar-free, non-existent literal must NOT be compiled into a
        // glob (which could silently expand to a non-empty set); it is the
        // CLI's loud `path does not exist`. Here it simply contributes nothing.
        let resolved = expand_search_paths(&[root.join("ghost.rs")], false, false);
        assert!(
            resolved.is_empty(),
            "metachar-free absent path is not glob-expanded: {resolved:?}"
        );
    }

    #[test]
    fn expand_search_paths_reporting_flags_only_zero_match_patterns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("real.rs"), "x").expect("write");

        // Arg 0: an existing file (renders, not a no-match).
        // Arg 1: a metachar pattern matching `real.rs` (matches, not a no-match).
        // Arg 2: a metachar pattern matching nothing → the sole no-match index.
        // Arg 3: a metachar-free absent (the CLI's `path does not exist`, NOT a
        //        pattern) → never reported here.
        let args = vec![
            root.join("real.rs"),
            root.join("*.rs"),
            root.join("*.none"),
            root.join("ghost.rs"),
        ];
        let (resolved, no_match) = expand_search_paths_reporting(&args, false, false);

        assert!(
            resolved.contains(&root.join("real.rs")),
            "existing file and matching pattern resolve: {resolved:?}"
        );
        assert_eq!(
            no_match,
            vec![2],
            "only the zero-match pattern (arg 2) is flagged: {no_match:?}"
        );
    }

    #[test]
    fn expand_search_paths_grouped_separates_patterns_from_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("real.rs"), "x").expect("write");
        std::fs::write(root.join("other.rs"), "x").expect("write");

        // Arg 0: an existing file (not a pattern, one resolved path).
        // Arg 1: a pattern matching both `.rs` files (a pattern, two matches).
        // Arg 2: a pattern matching nothing (a pattern, zero matches).
        // Arg 3: a metachar-free absent (not a pattern, contributes nothing).
        let args = vec![
            root.join("real.rs"),
            root.join("*.rs"),
            root.join("*.none"),
            root.join("ghost.rs"),
        ];
        let groups = expand_search_paths_grouped(&args, false, false);
        assert_eq!(groups.len(), 4, "one group per argument, in order");

        assert!(!groups[0].is_pattern, "existing file is not a pattern");
        assert_eq!(
            groups[0].resolved,
            vec![root.join("real.rs")],
            "{:?}",
            groups[0].resolved
        );

        assert!(groups[1].is_pattern, "metachar arg is a pattern");
        assert_eq!(groups[1].resolved.len(), 2, "{:?}", groups[1].resolved);
        assert!(groups[1].resolved.contains(&root.join("real.rs")));
        assert!(groups[1].resolved.contains(&root.join("other.rs")));

        assert!(groups[2].is_pattern, "zero-match arg is still a pattern");
        assert!(
            groups[2].resolved.is_empty(),
            "zero-match pattern contributes nothing: {:?}",
            groups[2].resolved
        );

        assert!(
            !groups[3].is_pattern,
            "metachar-free absent is not a pattern"
        );
        assert!(
            groups[3].resolved.is_empty(),
            "metachar-free absent contributes nothing: {:?}",
            groups[3].resolved
        );
    }

    // ── expand_glob_patterns_grouped_cancellable (VERBS one-verb form) ──

    #[test]
    fn expand_glob_patterns_every_group_is_a_pattern() {
        // The one-verb form has no name/pattern branch: every positional is a
        // pattern, `is_pattern: true`, always — a metachar-free self-matching
        // literal and a wildcard alike.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("real.rs"), "x").expect("write");
        std::fs::write(root.join("other.rs"), "x").expect("write");

        let args = vec![
            root.join("real.rs"),
            root.join("*.rs"),
            root.join("ghost.rs"),
        ];
        let groups = expand_glob_patterns_grouped_cancellable(
            &args,
            false,
            false,
            &CancellationToken::new(),
        );
        assert_eq!(groups.len(), 3, "one group per argument, in order");
        assert!(
            groups.iter().all(|g| g.is_pattern),
            "every group is a pattern"
        );

        // Arg 0: metachar-free existing file self-matches (exactly itself).
        assert_eq!(groups[0].resolved, vec![root.join("real.rs")]);
        // Arg 1: wildcard matches both `.rs` files.
        assert_eq!(groups[1].resolved.len(), 2, "{:?}", groups[1].resolved);
        // Arg 2: metachar-free absent is a zero-match pattern (not a "missing").
        assert!(
            groups[2].resolved.is_empty(),
            "a metachar-free absent is a zero-match pattern: {:?}",
            groups[2].resolved
        );
    }

    #[test]
    fn expand_glob_patterns_metachar_free_honors_gitignore() {
        // VERBS uniform gitignore: a metachar-free pattern pointing straight at
        // a gitignored file gets NO bypass — the file is filtered out (the
        // walker's file-root bypass is closed), and `--include-gitignored`
        // surfaces it. This is the "no metachar-free bypass" leg.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "secret.env\n").expect("write");
        std::fs::write(root.join("secret.env"), "K=V").expect("write");

        let arg = vec![root.join("secret.env")];

        let default =
            expand_glob_patterns_grouped_cancellable(&arg, false, false, &CancellationToken::new());
        assert!(
            default[0].resolved.is_empty(),
            "a metachar-free gitignored file is a zero-match, not a bypass: {:?}",
            default[0].resolved
        );

        let lifted =
            expand_glob_patterns_grouped_cancellable(&arg, true, false, &CancellationToken::new());
        assert_eq!(
            lifted[0].resolved,
            vec![root.join("secret.env")],
            "--include-gitignored is the one lever: {:?}",
            lifted[0].resolved
        );
    }

    #[test]
    fn expand_glob_patterns_metachar_free_dir_honors_gitignore() {
        // The bypass fix covers gitignored *directories* too (not just files): a
        // metachar-free `target` pattern where `target/` is gitignored is a
        // zero-match, and only `--include-gitignored` lists it. Rooting the walk
        // at a directory (the walker's own root) is where a naive fix would leak.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir(root.join("target")).expect("mkdir");
        std::fs::write(root.join("target/built.o"), "x").expect("write");

        let arg = vec![root.join("target")];

        let default =
            expand_glob_patterns_grouped_cancellable(&arg, false, false, &CancellationToken::new());
        assert!(
            default[0].resolved.is_empty(),
            "a metachar-free gitignored directory is a zero-match, not a bypass: {:?}",
            default[0].resolved
        );

        let lifted =
            expand_glob_patterns_grouped_cancellable(&arg, true, false, &CancellationToken::new());
        assert_eq!(
            lifted[0].resolved,
            vec![root.join("target")],
            "--include-gitignored surfaces the directory: {:?}",
            lifted[0].resolved
        );
    }

    #[test]
    fn expand_glob_patterns_metachar_free_hidden_needs_no_flag_for_explicit_dotname() {
        // misc-45 preserved natively: `*` does not cross a leading dot, but an
        // explicit dot-leading component matches without `--include-hidden`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::create_dir(root.join(".github")).expect("mkdir");
        std::fs::write(root.join(".github/ci.yml"), "x").expect("write");

        // Explicit `.github/ci.yml` self-matches with no hidden flag.
        let explicit = expand_glob_patterns_grouped_cancellable(
            &[root.join(".github/ci.yml")],
            false,
            false,
            &CancellationToken::new(),
        );
        assert_eq!(
            explicit[0].resolved,
            vec![root.join(".github/ci.yml")],
            "an explicit dot-leading component matches natively: {:?}",
            explicit[0].resolved
        );
    }

    // ── has_lsp_coverage ───────────────────────────────────────────

    /// Builds a `Session` rooted at `root` with the embedded default
    /// classification + server bindings loaded, so coverage gating sees the
    /// real served/unserved split.
    fn session_with_root(handle: &Handle, root: PathBuf) -> Session {
        let instance_id: Arc<str> = "test-session".into();
        Session::new(
            Config::default_with_classification(),
            vec![root],
            LoggingServer::new(),
            instance_id,
            handle.clone(),
            None,
        )
    }

    #[test]
    fn has_lsp_coverage_gates_in_root_on_served_language() {
        // Bug 44: the in-root tier must require the file's language to be
        // actually served, not blanket-cover every in-root path.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        // Served in-root types stay covered (rust → rust-analyzer).
        assert!(
            session.has_lsp_coverage(&root.join("src/main.rs")),
            "in-root .rs (served) must have coverage"
        );

        // Non-served in-root types flow free (no configured server).
        assert!(
            !session.has_lsp_coverage(&root.join("notes.txt")),
            "in-root .txt (non-served) must not claim coverage"
        );
        assert!(
            !session.has_lsp_coverage(&root.join("run.log")),
            "in-root .log (non-served) must not claim coverage"
        );
    }

    #[test]
    fn has_lsp_coverage_gates_out_of_root_on_single_file_coverage() {
        // Bug 44 / Decision 3: the out-of-root tier (tier 3) gates on single-file
        // coverage, NOT the in-root configured-server check. The project-based
        // rust-analyzer is not a single-file server, so an out-of-root .rs is not
        // covered — even though the same .rs in-root IS. Pins the `resolve_root`
        // in-root/out-of-root branch so it cannot collapse to a single tier.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        // In-root .rs is covered (configured server) ...
        assert!(
            session.has_lsp_coverage(&root.join("src/main.rs")),
            "in-root .rs must have coverage"
        );
        // ... but the same language out of root is not (no single-file server).
        let outside = tmp.path().join("outside").join("lib.rs");
        assert!(
            !session.has_lsp_coverage(&outside),
            "out-of-root .rs must gate on single-file coverage (none for rust)"
        );
    }

    // ── disable_lsp / disable_diag (ticket 00) ─────────────────────

    #[test]
    fn has_coverage_equals_lsp_coverage_without_linters() {
        // has_lint_coverage is a false stub (ticket 01), so has_coverage tracks
        // has_lsp_coverage exactly today.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        let served = root.join("src/main.rs");
        let unserved = root.join("notes.txt");
        assert_eq!(
            session.has_coverage(&served),
            session.has_lsp_coverage(&served),
            "has_coverage == has_lsp_coverage while linters are stubbed"
        );
        assert!(session.has_coverage(&served));
        assert!(!session.has_coverage(&unserved));
    }

    #[test]
    fn disable_lsp_root_has_no_lsp_coverage() {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        std::fs::write(root.join(".catenary.toml"), "[lsp]\ndisable = true\n")
            .expect("write config");
        // No spawn_all: `Session::new` primes the project config at construction,
        // so the gate sees `[lsp] disable` immediately (ticket 00).
        let session = session_with_root(rt.handle(), root.clone());

        let served = root.join("src/main.rs");
        // A served language in a disable_lsp root has NO LSP coverage ...
        assert!(
            !session.has_lsp_coverage(&served),
            "disable_lsp root must not claim LSP coverage"
        );
        // ... so with the linter stub still false, the editing gate is inert.
        assert!(
            !session.covered_for_diagnostics(&served),
            "disable_lsp root with no linter leaves the gate inert"
        );
    }

    #[test]
    fn disable_diag_root_keeps_lsp_coverage_but_gate_off() {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        std::fs::write(
            root.join(".catenary.toml"),
            "[diagnostics]\ndisable = true\n",
        )
        .expect("write config");
        // No spawn_all: the config is primed at construction (ticket 00).
        let session = session_with_root(rt.handle(), root.clone());

        let served = root.join("src/main.rs");
        // disable_diag keeps LSP navigation/coverage ...
        assert!(
            session.has_lsp_coverage(&served),
            "disable_diag keeps LSP coverage (navigation intact)"
        );
        assert!(session.diag_disabled(&served), "root is disable_diag");
        // ... but turns the diagnostics gate off.
        assert!(
            !session.covered_for_diagnostics(&served),
            "disable_diag turns the editing gate off despite LSP coverage"
        );
    }

    // ── has_lint_coverage (ticket 01) ──────────────────────────────

    #[test]
    fn has_lint_coverage_matches_configured_linter() {
        // A root-level `[linter.rule.*]` with a matching path glob covers a file
        // even when no language server backs it — the gate tracks lint-only files.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        std::fs::write(
            root.join(".catenary.toml"),
            "[linter.rule.shellcheck]\ncommand = \"shellcheck\"\n\
             args = [\"-f\", \"json1\"]\npatterns = [\"**/*.sh\"]\n",
        )
        .expect("write config");
        let session = session_with_root(rt.handle(), root.clone());

        let script = root.join("scripts/deploy.sh");
        // A .sh file in the root is lint-covered ...
        assert!(
            session.has_lint_coverage(&script),
            "configured shellcheck linter covers a matching .sh"
        );
        assert!(
            session.has_coverage(&script),
            "lint coverage feeds has_coverage"
        );
        // ... and gated for diagnostics (no LSP server required).
        assert!(
            session.covered_for_diagnostics(&script),
            "a lint-covered file is gated for diagnostics"
        );
        // A non-matching file is not lint-covered.
        assert!(
            !session.has_lint_coverage(&root.join("notes.txt")),
            "a non-matching file is not lint-covered"
        );
    }

    #[test]
    fn has_lint_coverage_from_default_linters() {
        // Ticket 03: the shipped default linters (defaults/linters.toml) are
        // inherited by every root, so a `.sh` file is lint-covered — and gated —
        // even with no `[linter.rule.*]` in user or project config. A file that
        // matches no default linter stays uncovered.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        // Default shellcheck (`**/*.sh`) covers a shell script with no explicit config.
        assert!(
            session.has_lint_coverage(&root.join("scripts/deploy.sh")),
            "the default shellcheck linter covers a .sh with no explicit config"
        );
        // A file matched by no default linter is uncovered.
        assert!(
            !session.has_lint_coverage(&root.join("notes.txt")),
            "a file matching no default linter is not lint-covered"
        );
    }
}
