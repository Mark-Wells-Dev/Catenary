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

use serde::Deserialize;

use crate::bridge::expand_tilde;

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
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
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
#[must_use]
pub fn expand_companions(declared: Vec<PathBuf>, rules: &CompanionRules) -> Vec<PathBuf> {
    if rules.is_empty() {
        return declared;
    }

    // Seed `seen` with the declared roots: a companion equal to a declared root
    // is skipped, and duplicate companions across roots collapse — both fall out
    // of the single `seen.insert` membership check below.
    let mut seen: HashSet<PathBuf> = declared.iter().cloned().collect();
    let mut result = declared.clone();

    for root in &declared {
        let canonical = canonical_project_root(root);
        for (matcher, template) in &rules.rules {
            if !matcher_matches(matcher, &canonical) {
                continue;
            }
            // `canonicalize` both existence-filters (Err ⇒ missing path) and
            // normalizes, so dedup/equality align with the canonical declared
            // roots.
            let Ok(resolved) = expand_template(template, &canonical).canonicalize() else {
                continue;
            };
            if resolved.is_dir() && seen.insert(resolved.clone()) {
                result.push(resolved);
            }
        }
    }

    result
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
fn canonical_project_root(r: &Path) -> PathBuf {
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
        CompanionRules, canonical_project_root, expand_companions, expand_env, expand_path_str,
        expand_template, matcher_matches,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

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
