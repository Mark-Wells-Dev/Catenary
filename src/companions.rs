// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Companion-root derivation (workstream 29).
//!
//! A *companion root* is a derived sibling directory auto-mounted alongside a
//! declared workspace root — the canonical case being the `{name}Internal`
//! planning repo next to a code checkout, so opening `~/Projects/Catenary` also
//! mounts `~/Projects/CatenaryInternal` for LSP intelligence.
//!
//! Companions ride the same MCP connection root set as any client-declared root
//! (see [`crate::router`]); this module owns only the *derivation*: turning a
//! declared root set plus a user-config rule table into the expanded set.
//!
//! Derivation is **silent on the agent surface but traced** at `debug!`
//! (`source = mcp.dispatch`) for the "user investigating" surface (logs/TUI):
//! every mount and every drop emits a structured event carrying the reason, so
//! "why isn't my companion mounting?" is answerable without a debugger. It never
//! emits `warn!`/`error!` — a dropped candidate is normal operation for a `*`
//! rule (most code checkouts have no `Internal` sibling), so it must not reach
//! the notification queue.
//!
//! Two properties shape the design:
//!
//! - **Off by default.** An empty [`CompanionRules`] is the identity transform —
//!   Catenary bakes in no naming convention.
//! - **Worktree-aware, no git dependency.** A linked worktree's companion derives
//!   from its *upstream project*, not its checkout path, by parsing git's own
//!   `.git`/`gitdir`/`commondir` pointer files with [`std::fs`] — no `git2`/`gix`
//!   crate, no subprocess.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::bridge::expand_tilde;
use crate::source::Source;

/// User-config companion-derivation rules: a matcher → template map.
///
/// Parsed from `[roots.companions]` in the **user** config
/// (`~/.config/catenary/config.toml`) — never a project `.catenary.toml`, so a
/// public repo cannot leak a private sibling path. Each entry maps a *matcher*
/// over the canonical declared root to a *companion template*:
///
/// ```toml
/// [roots.companions]
/// "*"                  = "{root}Internal"          # any root → its <path>Internal sibling
/// "~/Projects/homelab" = "~/.local/share/chezmoi"  # explicit, unrelated path
/// ```
///
/// - **Matcher** (key): `"*"` matches any root; any other value is a literal
///   path (after `~`/env expansion) matched for equality against the canonical
///   root.
/// - **Template** (value): `{root}` substitutes the canonical root path,
///   `{name}` its basename; `~` and `$VAR`/`${VAR}` expand. May also be a fully
///   literal path.
///
/// Semantics are **union**, not first-match — order is irrelevant. An empty
/// table (the default) disables the feature.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(transparent)]
pub struct CompanionRules {
    rules: HashMap<String, String>,
}

impl CompanionRules {
    /// Returns `true` when no rules are configured (feature off).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The configured `(matcher, template)` pairs, for a read-only consumer that
    /// wants to reason about the derivation without running it (the TUI's
    /// companion-nesting render — tui-rework 14, item 6a).
    ///
    /// This exposes only the parsed rule strings; the fs-touching derivation
    /// ([`expand_companions`]) stays the sole mount path.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rules.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Test constructor from `(matcher, template)` pairs.
    #[cfg(test)]
    #[must_use]
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            rules: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

/// Expands a declared root set with its configured companions.
///
/// Declared roots are always kept (first, in their original order). For each
/// declared root, its [`canonical_project_root`] is matched against every rule;
/// matching rules contribute a companion candidate. Candidates are **unioned**
/// into the result, **existence-filtered** (kept only if they resolve to an
/// existing directory), de-duplicated, and never equal to a declared root.
///
/// Derivation runs only on the *declared* set — companions never derive further
/// companions (no `FooInternalInternal`). An empty rule table returns `declared`
/// unchanged.
///
/// Every mount and every drop is traced at `debug!` (`source = mcp.dispatch`)
/// with the reason; see the module docs. Tracing is a side effect — the returned
/// set is unaffected, and nothing here reaches `warn!`/`error!`.
#[must_use]
pub fn expand_companions(declared: Vec<PathBuf>, rules: &CompanionRules) -> Vec<PathBuf> {
    if rules.is_empty() {
        return declared;
    }

    // Seed `seen` with the declared roots: a companion equal to a declared root
    // is skipped, and duplicate companions across roots collapse — both fall out
    // of the single `seen.insert` membership check below. `declared_set` is kept
    // alongside it so a drop can name *which* collision occurred (equal to a
    // declared root vs. already contributed by another root).
    let declared_set: HashSet<PathBuf> = declared.iter().cloned().collect();
    let mut seen = declared_set.clone();
    let mut result = declared.clone();

    for root in &declared {
        let canonical = canonical_project_root(root);
        let mut matched_any = false;
        for (matcher, template) in &rules.rules {
            if !matcher_matches(matcher, &canonical) {
                continue;
            }
            matched_any = true;

            // `canonicalize` both existence-filters (Err ⇒ missing path) and
            // normalizes, so dedup/equality align with the canonical declared
            // roots.
            let candidate = expand_template(template, &canonical);
            let Ok(resolved) = candidate.canonicalize() else {
                trace_drop(
                    root,
                    &canonical,
                    Some(matcher),
                    Some(&candidate),
                    "not an existing directory",
                );
                continue;
            };
            if !resolved.is_dir() {
                trace_drop(
                    root,
                    &canonical,
                    Some(matcher),
                    Some(&resolved),
                    "not an existing directory",
                );
                continue;
            }
            if declared_set.contains(&resolved) {
                trace_drop(
                    root,
                    &canonical,
                    Some(matcher),
                    Some(&resolved),
                    "equals declared root",
                );
                continue;
            }
            if !seen.insert(resolved.clone()) {
                trace_drop(
                    root,
                    &canonical,
                    Some(matcher),
                    Some(&resolved),
                    "duplicate companion",
                );
                continue;
            }
            debug!(
                source = Source::McpDispatch.as_str(),
                declared = %root.display(),
                canonical = %canonical.display(),
                matcher = matcher.as_str(),
                companion = %resolved.display(),
                "companion mounted",
            );
            result.push(resolved);
        }
        if !matched_any {
            trace_drop(root, &canonical, None, None, "no rule matched");
        }
    }

    debug!(
        source = Source::McpDispatch.as_str(),
        roots = declared.len(),
        mounted = result.len() - declared.len(),
        "companion derivation complete",
    );

    result
}

/// Emits the `debug!` trace for a dropped companion candidate.
///
/// `matcher`/`candidate` are `None` only for the `"no rule matched"` outcome,
/// where neither is known; they render as empty fields. Lives at `debug!`
/// (never `warn!`/`error!`) — a dropped candidate is normal operation.
fn trace_drop(
    declared: &Path,
    canonical: &Path,
    matcher: Option<&str>,
    candidate: Option<&Path>,
    reason: &'static str,
) {
    let candidate = candidate
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    debug!(
        source = Source::McpDispatch.as_str(),
        declared = %declared.display(),
        canonical = %canonical.display(),
        matcher = matcher.unwrap_or_default(),
        candidate = candidate.as_str(),
        reason,
        "companion candidate dropped",
    );
}

/// Resolves a declared root to its **canonical project root** — for a linked
/// git worktree, the main worktree; otherwise the root itself.
///
/// A worktree's companion derives from its upstream project, not its checkout
/// path (`…/Worktrees/Catenary-bug24` → `…/Catenary`, deriving
/// `…/CatenaryInternal`, not `…Catenary-bug24Internal`).
///
/// Resolution parses git's own worktree-pointer files via [`std::fs`] — no git
/// crate, no subprocess:
///
/// - `r/.git` is a **directory** ⇒ `r` (normal checkout / main worktree).
/// - `r/.git` is a **file** `gitdir: <G>`:
///   - `<G>` sits under a `worktrees/` dir ⇒ linked worktree: read
///     `<G>/commondir`, resolve it against `<G>` to the common `.git` dir, whose
///     **parent** is the canonical project root.
///   - otherwise (e.g. `<G>` under `modules/` = submodule) ⇒ `r`.
/// - no `r/.git` ⇒ `r`.
///
/// The canonical root is used **only** to derive the companion — it is never
/// itself mounted. Any parse/IO failure falls back to `r`.
///
/// **Limitation — a linked worktree of a `--separate-git-dir` repo.** The
/// worktree branch takes the **parent** of the common git dir as the project
/// root, which holds only when that git dir lives *inside* the main worktree
/// (the standard layout). When the git dir is relocated outside the working tree
/// (`--separate-git-dir`, used in dotfiles/`yadm`-style setups), the parent is an
/// unrelated directory, and git records the original working tree nowhere
/// reachable from here — not in the common dir's `config` (`core.worktree` is
/// unset by `git init --separate-git-dir`), not under `worktrees/` (which lists
/// only *linked* worktrees). `git` itself can't recover it from this side, so no
/// `std::fs` parse (or git crate) could either. That intersection — relocated git
/// dir **and** a linked worktree of it — is unsupported and resolves wrongly;
/// in practice it degrades to a no-op (the wrong path's sibling won't exist, so
/// the existence filter drops the companion). Every other case — normal
/// checkouts, standard linked worktrees, submodules, and a *directly-opened*
/// `--separate-git-dir` main worktree (its gitdir is not under `worktrees/`, so
/// it returns `r`) — resolves correctly.
#[must_use]
pub fn canonical_project_root(r: &Path) -> PathBuf {
    let dot_git = r.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
        return r.to_path_buf(); // no `.git`
    };
    if meta.is_dir() {
        return r.to_path_buf(); // normal checkout / main worktree
    }

    // `.git` is a file → `gitdir: <G>`.
    let Some(gitdir) = read_gitdir(&dot_git) else {
        return r.to_path_buf();
    };
    let g = resolve_against(r, &gitdir);

    // Linked worktree iff <G> == <common>/worktrees/<name>. A submodule's gitdir
    // sits under `modules/` instead, which we treat as its own project.
    let is_worktree = g
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "worktrees");
    if !is_worktree {
        return r.to_path_buf();
    }

    let Ok(common_raw) = std::fs::read_to_string(g.join("commondir")) else {
        return r.to_path_buf();
    };
    let common_git = resolve_against(&g, common_raw.trim());
    common_git.canonicalize().map_or_else(
        |_| r.to_path_buf(),
        |canon| {
            canon
                .parent()
                .map_or_else(|| r.to_path_buf(), Path::to_path_buf)
        },
    )
}

/// Version-control root markers, in probe order.
///
/// A directory carrying any of these is a repository / working-copy toplevel:
/// `.git` (Git — a **directory** for a normal checkout / main worktree, or a
/// **file** carrying a `gitdir:` pointer for a linked worktree), `.svn`
/// (Subversion), `.hg` (Mercurial), or `.jj` (Jujutsu). Detection is
/// marker-presence only; git-worktree linkage resolution (see [`read_gitdir`]
/// and [`canonical_project_root`]) stays git-specific.
const REPO_MARKERS: [&str; 4] = [".git", ".svn", ".hg", ".jj"];

/// Resolves a file path to the toplevel of its enclosing repository.
///
/// Walks the path's ancestors looking for a version-control root marker
/// ([`REPO_MARKERS`]): `.git` — a **directory** (normal checkout / main
/// worktree) or a **file** (`gitdir:` pointer for a linked worktree) — or a
/// `.svn`/`.hg`/`.jj` directory. The first ancestor that carries any marker is
/// the repository toplevel; where markers nest, the walk resolves to the
/// *nearest* enclosing one. Returns `None` if no ancestor carries a marker (the
/// file is outside any repository).
///
/// This is the worktree-root analogue of [`canonical_project_root`]: the latter
/// maps a worktree root to its *canonical project* (git-worktree linkage only);
/// this maps a *file* to the repository root that [`canonical_project_root`]
/// then resolves. Used by the auto-mount path to find the repository an edited
/// file lives in. Parsing is pure [`std::fs`] — no VCS crate, no subprocess —
/// and tolerates a non-existent marker (the ancestor simply doesn't match).
#[must_use]
pub fn enclosing_worktree_root(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .find(|dir| {
            REPO_MARKERS
                .iter()
                .any(|marker| std::fs::symlink_metadata(dir.join(marker)).is_ok())
        })
        .map(Path::to_path_buf)
}

/// Reads the `gitdir: <path>` pointer from a `.git` file.
fn read_gitdir(dot_git: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(dot_git).ok()?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(|p| p.trim().to_string())
}

/// Resolves `p` against `base` when relative; returns it as-is when absolute.
fn resolve_against(base: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Returns `true` if `matcher` selects `canonical`.
///
/// `"*"` matches any root. Any other matcher is a literal path: it is `~`/env
/// expanded, then compared with the canonical root — directly, or after
/// canonicalizing the matcher when it exists (tolerating symlinks and trailing
/// slashes).
fn matcher_matches(matcher: &str, canonical: &Path) -> bool {
    if matcher.trim() == "*" {
        return true;
    }
    let expanded = PathBuf::from(expand_path_str(matcher));
    expanded == canonical || matches!(expanded.canonicalize(), Ok(c) if c == canonical)
}

/// Builds a companion path from a template and a canonical root.
///
/// Substitutes `{root}` (the canonical root path) and `{name}` (its basename),
/// then expands `~` and environment variables.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`{root}`/`{name}` are companion-template placeholders, not format args"
)]
fn expand_template(template: &str, canonical: &Path) -> PathBuf {
    let root = canonical.to_string_lossy();
    let name = canonical
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let substituted = template.replace("{root}", &root).replace("{name}", &name);
    PathBuf::from(expand_path_str(&substituted))
}

/// Expands `~`/`~/` (home) and `$VAR`/`${VAR}` (environment) in a path string.
fn expand_path_str(s: &str) -> String {
    expand_tilde(&expand_env(s))
}

/// Expands `$VAR` and `${VAR}` environment references.
///
/// Undefined variables expand to empty (shell semantics). A `$` that does not
/// begin a valid reference is preserved literally.
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                for ch in chars.by_ref() {
                    if ch == '}' {
                        closed = true;
                        break;
                    }
                    name.push(ch);
                }
                if closed && !name.is_empty() {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    // Malformed reference — preserve what we consumed literally.
                    out.push_str("${");
                    out.push_str(&name);
                    if closed {
                        out.push('}');
                    }
                }
            }
            Some(&ch) if ch == '_' || ch.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch == '_' || ch.is_ascii_alphanumeric() {
                        name.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            _ => out.push('$'), // lone `$`
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`{root}`/`${VAR}` test inputs are template placeholders, not format args"
)]
mod tests {
    use super::{
        CompanionRules, canonical_project_root, enclosing_worktree_root, expand_companions,
        expand_env, expand_path_str, expand_template, matcher_matches,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use tempfile::tempdir;
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;

    /// Canonicalizes a path, failing the test on error (test-only).
    fn canon(p: &Path) -> PathBuf {
        p.canonicalize().expect("canonicalize path")
    }

    // ── canonical_project_root ────────────────────────────────────────────

    #[test]
    fn canonical_project_root_normal_checkout_is_self() {
        let dir = tempdir().expect("tempdir");
        let root = canon(dir.path());
        fs::create_dir(root.join(".git")).expect("mkdir .git");

        assert_eq!(canonical_project_root(&root), root);
    }

    #[test]
    fn canonical_project_root_no_git_is_self() {
        let dir = tempdir().expect("tempdir");
        let root = canon(dir.path());

        assert_eq!(canonical_project_root(&root), root);
    }

    #[test]
    fn canonical_project_root_linked_worktree_is_main() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());

        // Main project: `<base>/project/.git/worktrees/wt/commondir` → "../.."
        let project = base.join("project");
        let wt_gitdir = project.join(".git").join("worktrees").join("wt");
        fs::create_dir_all(&wt_gitdir).expect("mkdir worktree gitdir");
        fs::write(wt_gitdir.join("commondir"), "../..\n").expect("write commondir");

        // Linked worktree: `<base>/checkout/.git` is a file pointing at it.
        let checkout = base.join("checkout");
        fs::create_dir(&checkout).expect("mkdir checkout");
        fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .expect("write .git file");

        assert_eq!(canonical_project_root(&checkout), project);
    }

    // ── enclosing_worktree_root ───────────────────────────────────────────

    #[test]
    fn enclosing_worktree_root_finds_dir_dot_git() {
        let dir = tempdir().expect("tempdir");
        let root = canon(dir.path());
        fs::create_dir(root.join(".git")).expect("mkdir .git");
        let src = root.join("src");
        fs::create_dir(&src).expect("mkdir src");
        let file = src.join("lib.rs");
        fs::write(&file, "").expect("write file");

        assert_eq!(enclosing_worktree_root(&file), Some(root));
    }

    #[test]
    fn enclosing_worktree_root_finds_file_dot_git() {
        // A linked worktree's toplevel carries a `.git` *file*, not a dir.
        let dir = tempdir().expect("tempdir");
        let root = canon(dir.path());
        fs::write(root.join(".git"), "gitdir: /somewhere\n").expect("write .git file");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).expect("mkdir nested");
        let file = nested.join("mod.rs");
        fs::write(&file, "").expect("write file");

        assert_eq!(enclosing_worktree_root(&file), Some(root));
    }

    #[test]
    fn enclosing_worktree_root_finds_nongit_markers() {
        // A project rooted by any non-git VCS marker is detected just like `.git`.
        for marker in [".svn", ".hg", ".jj"] {
            let dir = tempdir().expect("tempdir");
            let root = canon(dir.path());
            fs::create_dir(root.join(marker)).expect("mkdir marker");
            let src = root.join("src");
            fs::create_dir(&src).expect("mkdir src");
            let file = src.join("lib.rs");
            fs::write(&file, "").expect("write file");

            assert_eq!(
                enclosing_worktree_root(&file),
                Some(root),
                "marker {marker}"
            );
        }
    }

    #[test]
    fn enclosing_worktree_root_picks_nearest_marker() {
        // An inner `.hg` project nested inside an outer `.git` checkout: the walk
        // stops at the nearest enclosing marker, not the outermost.
        let dir = tempdir().expect("tempdir");
        let outer = canon(dir.path());
        fs::create_dir(outer.join(".git")).expect("mkdir outer .git");
        let inner = outer.join("vendor").join("dep");
        fs::create_dir_all(&inner).expect("mkdir inner");
        fs::create_dir(inner.join(".hg")).expect("mkdir inner .hg");
        let src = inner.join("src");
        fs::create_dir(&src).expect("mkdir src");
        let file = src.join("mod.rs");
        fs::write(&file, "").expect("write file");

        assert_eq!(enclosing_worktree_root(&file), Some(inner));
    }

    #[test]
    fn enclosing_worktree_root_none_outside_checkout() {
        let dir = tempdir().expect("tempdir");
        let root = canon(dir.path());
        let file = root.join("loose.rs");
        fs::write(&file, "").expect("write file");

        assert_eq!(enclosing_worktree_root(&file), None);
    }

    #[test]
    fn canonical_project_root_submodule_is_self() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());

        // Submodule gitdir lives under `modules/`, not `worktrees/`.
        let super_modules = base.join("super").join(".git").join("modules").join("sub");
        fs::create_dir_all(&super_modules).expect("mkdir submodule gitdir");

        let submodule = base.join("sub");
        fs::create_dir(&submodule).expect("mkdir submodule checkout");
        fs::write(
            submodule.join(".git"),
            format!("gitdir: {}\n", super_modules.display()),
        )
        .expect("write .git file");

        assert_eq!(canonical_project_root(&submodule), submodule);
    }

    // ── expand_companions ─────────────────────────────────────────────────

    #[test]
    fn empty_rules_is_identity() {
        let dir = tempdir().expect("tempdir");
        let root = canon(dir.path());
        let declared = vec![root];

        let out = expand_companions(declared.clone(), &CompanionRules::default());
        assert_eq!(out, declared);
    }

    #[test]
    fn star_rule_mounts_existing_internal_sibling() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let foo = base.join("Foo");
        let foo_internal = base.join("FooInternal");
        fs::create_dir(&foo).expect("mkdir Foo");
        fs::create_dir(&foo_internal).expect("mkdir FooInternal");

        let rules = CompanionRules::from_pairs([("*", "{root}Internal")]);
        let out = expand_companions(vec![foo.clone()], &rules);

        assert_eq!(out, vec![foo, foo_internal]);
    }

    #[test]
    fn star_rule_drops_nonexistent_sibling() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let foo = base.join("Foo");
        fs::create_dir(&foo).expect("mkdir Foo");
        // No `FooInternal` on disk.

        let rules = CompanionRules::from_pairs([("*", "{root}Internal")]);
        let out = expand_companions(vec![foo.clone()], &rules);

        assert_eq!(out, vec![foo]);
    }

    #[test]
    fn literal_matcher_fires_only_for_its_root() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let homelab = base.join("homelab");
        let chezmoi = base.join("chezmoi");
        let other = base.join("other");
        fs::create_dir(&homelab).expect("mkdir homelab");
        fs::create_dir(&chezmoi).expect("mkdir chezmoi");
        fs::create_dir(&other).expect("mkdir other");

        let rules = CompanionRules::from_pairs([(
            homelab.to_string_lossy().into_owned(),
            chezmoi.to_string_lossy().into_owned(),
        )]);

        // The matching root pulls in its explicit companion.
        let out = expand_companions(vec![homelab.clone()], &rules);
        assert_eq!(out, vec![homelab, chezmoi]);

        // A non-matching root gets nothing.
        let out = expand_companions(vec![other.clone()], &rules);
        assert_eq!(out, vec![other]);
    }

    #[test]
    fn companion_equal_to_declared_is_not_duplicated() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let foo = base.join("Foo");
        fs::create_dir(&foo).expect("mkdir Foo");

        // The template resolves the companion back to the declared root itself.
        let rules = CompanionRules::from_pairs([("*", "{root}")]);
        let out = expand_companions(vec![foo.clone()], &rules);

        assert_eq!(out, vec![foo]);
    }

    #[test]
    fn two_roots_resolving_same_companion_dedup() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let a = base.join("a");
        let b = base.join("b");
        let shared = base.join("shared");
        fs::create_dir(&a).expect("mkdir a");
        fs::create_dir(&b).expect("mkdir b");
        fs::create_dir(&shared).expect("mkdir shared");

        let rules =
            CompanionRules::from_pairs([("*".to_string(), shared.to_string_lossy().into_owned())]);
        let out = expand_companions(vec![a.clone(), b.clone()], &rules);

        assert_eq!(out, vec![a, b, shared]);
    }

    #[test]
    fn worktree_root_derives_project_companion() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());

        // Main project `<base>/Catenary` with a linked worktree pointer.
        let project = base.join("Catenary");
        let wt_gitdir = project.join(".git").join("worktrees").join("wt");
        fs::create_dir_all(&wt_gitdir).expect("mkdir worktree gitdir");
        fs::write(wt_gitdir.join("commondir"), "../..\n").expect("write commondir");

        // The companion is the *project's* Internal sibling, which exists.
        let companion = base.join("CatenaryInternal");
        fs::create_dir(&companion).expect("mkdir CatenaryInternal");

        // The checked-out worktree the agent actually works in.
        let worktree = base.join("Catenary-companion");
        fs::create_dir(&worktree).expect("mkdir worktree");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .expect("write .git file");

        let rules = CompanionRules::from_pairs([("*", "{root}Internal")]);
        let out = expand_companions(vec![worktree.clone()], &rules);

        // Worktree stays; companion is `CatenaryInternal`, NOT
        // `Catenary-companionInternal`.
        assert_eq!(out, vec![worktree, companion]);
    }

    // ── derivation tracing ────────────────────────────────────────────────

    /// One captured tracing event: its level and string-valued fields.
    #[derive(Clone, Debug)]
    struct CapturedEvent {
        level: Level,
        fields: HashMap<String, String>,
    }

    /// Minimal tracing layer that records every event's level and fields.
    struct CaptureLayer {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(HashMap<String, String>);
            impl tracing::field::Visit for Visitor {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .insert(field.name().to_string(), format!("{value:?}"));
                }
            }
            let mut v = Visitor(HashMap::new());
            event.record(&mut v);
            if let Ok(mut events) = self.events.lock() {
                events.push(CapturedEvent {
                    level: *event.metadata().level(),
                    fields: v.0,
                });
            }
        }
    }

    /// Runs `expand_companions` under a capturing subscriber, returning the
    /// derived set together with every tracing event it emitted.
    fn capture(
        declared: Vec<PathBuf>,
        rules: &CompanionRules,
    ) -> (Vec<PathBuf>, Vec<CapturedEvent>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            events: Arc::clone(&events),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let out =
            tracing::subscriber::with_default(subscriber, || expand_companions(declared, rules));
        let captured = events.lock().expect("lock captured events").clone();
        (out, captured)
    }

    /// The level-discipline invariant: derivation must never reach the
    /// notification queue.
    fn assert_no_warn_or_error(events: &[CapturedEvent]) {
        assert!(
            events
                .iter()
                .all(|e| e.level != Level::WARN && e.level != Level::ERROR),
            "companion derivation must stay at debug; found warn/error: {events:?}",
        );
    }

    #[test]
    fn mount_emits_debug_event_carrying_companion() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let foo = base.join("Foo");
        let foo_internal = base.join("FooInternal");
        fs::create_dir(&foo).expect("mkdir Foo");
        fs::create_dir(&foo_internal).expect("mkdir FooInternal");

        let rules = CompanionRules::from_pairs([("*", "{root}Internal")]);
        let (out, events) = capture(vec![foo.clone()], &rules);

        assert_eq!(out, vec![foo, foo_internal.clone()]);
        let mount = events
            .iter()
            .find(|e| e.fields.contains_key("companion"))
            .expect("a mount event carrying `companion`");
        assert_eq!(mount.level, Level::DEBUG);
        assert_eq!(
            mount.fields.get("companion"),
            Some(&foo_internal.display().to_string()),
        );
        assert_no_warn_or_error(&events);
    }

    #[test]
    fn nonexistent_sibling_emits_not_an_existing_directory() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let foo = base.join("Foo");
        fs::create_dir(&foo).expect("mkdir Foo");
        // No `FooInternal` on disk.

        let rules = CompanionRules::from_pairs([("*", "{root}Internal")]);
        let (_out, events) = capture(vec![foo], &rules);

        assert!(
            events.iter().any(|e| e.level == Level::DEBUG
                && e.fields.get("reason").map(String::as_str) == Some("not an existing directory")),
            "expected a debug drop with reason 'not an existing directory', got: {events:?}",
        );
        assert_no_warn_or_error(&events);
    }

    #[test]
    fn companion_equal_to_declared_emits_equals_declared_root() {
        let dir = tempdir().expect("tempdir");
        let base = canon(dir.path());
        let foo = base.join("Foo");
        fs::create_dir(&foo).expect("mkdir Foo");

        // The template resolves the companion back to the declared root itself.
        let rules = CompanionRules::from_pairs([("*", "{root}")]);
        let (_out, events) = capture(vec![foo], &rules);

        assert!(
            events.iter().any(|e| e.level == Level::DEBUG
                && e.fields.get("reason").map(String::as_str) == Some("equals declared root")),
            "expected a debug drop with reason 'equals declared root', got: {events:?}",
        );
        assert_no_warn_or_error(&events);
    }

    // ── helpers ───────────────────────────────────────────────────────────

    #[test]
    fn expand_template_substitutes_root_and_name() {
        let root = Path::new("/p/Foo");
        assert_eq!(
            expand_template("{root}Internal", root),
            PathBuf::from("/p/FooInternal"),
        );
        assert_eq!(
            expand_template("/docs/{name}", root),
            PathBuf::from("/docs/Foo")
        );
    }

    #[test]
    fn matcher_star_matches_anything() {
        assert!(matcher_matches("*", Path::new("/anything")));
        assert!(matcher_matches("  *  ", Path::new("/anything")));
    }

    #[test]
    fn matcher_literal_compares_paths() {
        assert!(matcher_matches("/p/Foo", Path::new("/p/Foo")));
        assert!(!matcher_matches("/p/Bar", Path::new("/p/Foo")));
    }

    #[test]
    fn expand_path_str_expands_home() {
        let Ok(home) = std::env::var("HOME") else {
            return; // no HOME in this environment — skip
        };
        assert_eq!(expand_path_str("~/x"), format!("{home}/x"));
    }

    #[test]
    fn expand_env_reads_existing_var() {
        let Ok(home) = std::env::var("HOME") else {
            return; // no HOME in this environment — skip
        };
        assert_eq!(expand_env("${HOME}/x"), format!("{home}/x"));
        assert_eq!(expand_env("$HOME/x"), format!("{home}/x"));
    }

    #[test]
    fn expand_env_preserves_lone_and_unknown() {
        // A bare `$` with no identifier is preserved.
        assert_eq!(expand_env("a $ b"), "a $ b");
        // An undefined variable expands to empty (shell semantics).
        assert_eq!(expand_env("x${CATENARY_DEFINITELY_UNSET_XYZ}y"), "xy");
    }
}
