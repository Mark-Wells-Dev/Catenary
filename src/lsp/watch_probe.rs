// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Supplemental watch-observation probe planning (bug 143).
//!
//! Catenary has no OS watcher: the observation sets that feed
//! [`nudge_changed_set`](crate::lsp::manager::LspClientManager::nudge_changed_set)
//! come exclusively from Catenary's own walks, and every one of those walks is
//! built in **search posture** — `git_ignore(true).hidden(true)`, right for
//! `grep`/`glob` and wrong for a subsystem whose job is to tell servers what
//! changed on disk. Three path classes are therefore structurally unobservable
//! no matter what a server registered: dotfiles (`**/.lattice.toml`), gitignored
//! paths (rust-analyzer's `baseUri` watchers on `target/…/out`), and paths
//! outside every workspace root.
//!
//! The maintainer's ruling (bug 143, 2026-07-29) is **registration-driven and
//! unconditional**: the union of registered watcher globs defines a supplemental
//! observation leg served with the search filters (hidden, gitignore) **off**.
//! The main walk keeps its search posture untouched — it is never de-filtered.
//!
//! This module answers the planning half of that ruling: given one registered
//! glob, *what should we look at?* A compiled matcher can only answer "does this
//! path match?", so the plan is derived from the pattern's **source text** at
//! registration time and cached on the watcher. Three probe forms, cheapest
//! first:
//!
//! - [`paths`](WatchProbe::paths) — a fully literal pattern (`Cargo.lock`,
//!   `build/compile_commands.json`, or a `baseUri`-anchored literal) is one
//!   candidate path: a direct stat.
//! - [`suffixes`](WatchProbe::suffixes) — a `**/`-prefixed literal remainder
//!   (`**/.lattice.toml`, `**/Cargo.toml`) is a *marker*: stat it at the root
//!   and inside each directory the main walk already visited.
//! - [`dirs`](WatchProbe::dirs) — a `baseUri`-anchored pattern that genuinely
//!   needs recursion (`{ baseUri: …/out, pattern: "**/*" }`) is a bounded
//!   de-filtered walk of that one server-named directory.
//!
//! A pattern whose *name* part is wildcarded and which is not `baseUri`-anchored
//! (`**/*.rs`, `**/*.md`) plans **nothing**: serving it would mean a second,
//! de-filtered full walk of the root — the "targeted stats, not a second full
//! walk" bound. Such patterns are exactly the ones the main walk already serves
//! in its search posture; what they lose is only the hidden/ignored tail.

use std::path::{Path, PathBuf};

use super::glob::GlobPattern;

/// Cap on the alternatives one brace expansion may produce.
///
/// `**/Cargo.{lock,toml}` yields 2; a pathological nest could yield thousands.
/// Past the cap the pattern plans nothing rather than exploding — the main walk
/// still serves it in search posture.
const MAX_ALTERNATIVES: usize = 64;

/// Returns whether `s` is free of glob metacharacters — a literal path fragment.
///
/// `{`/`}` count as metacharacters here because brace expansion runs *before*
/// this check: anything still carrying a brace was rejected by the expander.
fn is_literal(s: &str) -> bool {
    !s.contains(['*', '?', '[', ']', '{', '}', '\\'])
}

/// Brace-expands `pattern` into its literal alternatives.
///
/// `**/Cargo.{lock,toml}` → `["**/Cargo.lock", "**/Cargo.toml"]`. Nested braces
/// expand recursively. Returns `None` — meaning "plan nothing for this pattern"
/// — when the input carries a character class (`[`/`]`) or an escape (`\`),
/// whose commas and braces this deliberately small expander would misread, when
/// the braces are unbalanced, or when the expansion would exceed
/// [`MAX_ALTERNATIVES`].
fn expand_braces(pattern: &str) -> Option<Vec<String>> {
    if pattern.contains(['[', ']', '\\']) {
        return None;
    }
    let mut out = Vec::new();
    expand_into(pattern, &mut out)?;
    Some(out)
}

/// Recursive worker for [`expand_braces`]: expands the first top-level brace
/// group and recurses on each resulting string.
fn expand_into(pattern: &str, out: &mut Vec<String>) -> Option<()> {
    let Some(open) = pattern.find('{') else {
        if pattern.contains('}') {
            return None;
        }
        if out.len() >= MAX_ALTERNATIVES {
            return None;
        }
        out.push(pattern.to_string());
        return Some(());
    };

    // Find this group's matching close brace, tracking nesting.
    let mut depth = 0usize;
    let mut close = None;
    for (idx, ch) in pattern[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    close = Some(open + idx);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;

    // Split the group body on top-level commas only.
    let body = &pattern[open + 1..close];
    let mut alternatives: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in body.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 => {
                alternatives.push(&body[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    alternatives.push(&body[start..]);

    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    for alternative in alternatives {
        expand_into(&format!("{prefix}{alternative}{suffix}"), out)?;
    }
    Some(())
}

/// The supplemental observation plan for one registered watcher glob.
///
/// Empty (the [`Default`]) means "the main walk's search posture is all this
/// pattern gets". Built once per watcher at registration time (see
/// [`crate::lsp::server::ParsedWatcher`]) and read on every nudge, so the
/// derivation cost is paid once per registration, never per walk.
#[derive(Clone, Debug, Default)]
pub struct WatchProbe {
    /// Literal candidate paths. An absolute entry is probed as-is; a relative
    /// entry is resolved against the workspace root.
    paths: Vec<PathBuf>,
    /// Literal relative suffixes to probe under the workspace root and under
    /// each directory the main walk already visited.
    suffixes: Vec<PathBuf>,
    /// Directories to enumerate with the search filters off.
    dirs: Vec<PathBuf>,
}

impl WatchProbe {
    /// Derives the supplemental observation plan for one registered glob.
    ///
    /// See the module documentation for the three probe forms and the
    /// deliberate "plans nothing" case.
    #[must_use]
    pub fn derive(pattern: &GlobPattern) -> Self {
        let mut probe = Self::default();
        match pattern {
            GlobPattern::Plain(glob) => {
                let Some(alternatives) = expand_braces(glob.source()) else {
                    return probe;
                };
                for alternative in alternatives {
                    probe.plan_workspace_relative(&alternative);
                }
            }
            GlobPattern::Relative {
                base,
                pattern: base_relative,
            } => {
                let Some(alternatives) = expand_braces(base_relative.source()) else {
                    // Un-analyzable under a server-named base: the base itself
                    // is the bound, so walk it rather than plan nothing.
                    probe.dirs.push(base.clone());
                    return probe;
                };
                for alternative in alternatives {
                    probe.plan_base_relative(base, &alternative);
                }
            }
        }
        probe.dirs.sort_unstable();
        probe.dirs.dedup();
        probe
    }

    /// Plans one workspace-relative alternative (a `Plain` pattern).
    fn plan_workspace_relative(&mut self, alternative: &str) {
        if let Some(remainder) = alternative.strip_prefix("**/") {
            // `**/NAME` — a marker: the name can sit in any directory, so it is
            // probed at the root and in every directory the main walk visited.
            if !remainder.is_empty() && is_literal(remainder) {
                self.suffixes.push(PathBuf::from(remainder));
            }
        } else if is_literal(alternative) && !alternative.is_empty() {
            // A fully literal workspace-relative (or absolute) path: one stat.
            self.paths.push(PathBuf::from(alternative));
        }
        // Anything else (`**/*.rs`, `src/*.json`) plans nothing: serving it
        // would mean a second de-filtered full walk of the root.
    }

    /// Plans one `baseUri`-relative alternative (a `Relative` pattern).
    ///
    /// The base is a literal directory the server named, so a wildcarded
    /// remainder is bounded by that directory — the one case where a
    /// de-filtered walk is warranted.
    fn plan_base_relative(&mut self, base: &Path, alternative: &str) {
        if is_literal(alternative) && !alternative.is_empty() {
            self.paths.push(base.join(alternative));
        } else {
            self.dirs.push(base.to_path_buf());
        }
    }

    /// Returns whether this plan asks for nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.suffixes.is_empty() && self.dirs.is_empty()
    }

    /// Literal candidate paths — absolute, or relative to the workspace root.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Literal marker suffixes, probed under the root and each walked directory.
    #[must_use]
    pub fn suffixes(&self) -> &[PathBuf] {
        &self.suffixes
    }

    /// Directories to enumerate with the hidden/gitignore filters off.
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use serde_json::json;

    fn plain(pattern: &str) -> GlobPattern {
        GlobPattern::from_value(&json!(pattern)).expect("valid pattern")
    }

    fn relative(base: &str, pattern: &str) -> GlobPattern {
        GlobPattern::from_value(&json!({ "baseUri": format!("file://{base}"), "pattern": pattern }))
            .expect("valid pattern")
    }

    // ── brace expansion ──────────────────────────────────────────────

    #[test]
    fn expand_braces_splits_top_level_alternatives() {
        let alternatives = expand_braces("**/Cargo.{lock,toml}").expect("expandable");
        assert_eq!(alternatives, vec!["**/Cargo.lock", "**/Cargo.toml"]);
    }

    #[test]
    fn expand_braces_handles_nesting() {
        let alternatives = expand_braces("a.{b,c{d,e}}").expect("expandable");
        assert_eq!(alternatives, vec!["a.b", "a.cd", "a.ce"]);
    }

    #[test]
    fn expand_braces_passes_through_brace_free_patterns() {
        let alternatives = expand_braces("**/.lattice.toml").expect("expandable");
        assert_eq!(alternatives, vec!["**/.lattice.toml"]);
    }

    #[test]
    fn expand_braces_refuses_character_classes_and_escapes() {
        // A `[…]` body may carry commas/braces this small expander would
        // misread, and `\{` is a literal brace — both plan nothing.
        assert!(expand_braces("example.[0-9]").is_none());
        assert!(expand_braces("a\\{b").is_none());
    }

    #[test]
    fn expand_braces_refuses_unbalanced_braces() {
        assert!(expand_braces("a{b").is_none());
        assert!(expand_braces("a}b").is_none());
    }

    // ── plan derivation ──────────────────────────────────────────────

    #[test]
    fn dotfile_marker_plans_a_suffix_probe() {
        // The bug-143 incident pattern: hidden at every depth, so the main
        // (hidden-skipping) walk can never observe it.
        let probe = WatchProbe::derive(&plain("**/.lattice.toml"));
        assert_eq!(probe.suffixes(), [PathBuf::from(".lattice.toml")]);
        assert!(probe.paths().is_empty());
        assert!(probe.dirs().is_empty());
    }

    #[test]
    fn brace_marker_plans_one_suffix_per_alternative() {
        let probe = WatchProbe::derive(&plain("**/Cargo.{lock,toml}"));
        assert_eq!(
            probe.suffixes(),
            [PathBuf::from("Cargo.lock"), PathBuf::from("Cargo.toml")]
        );
    }

    #[test]
    fn literal_workspace_path_plans_a_direct_stat() {
        // The gitignored-path class: a literal path inside an ignored tree is
        // one stat, no walk.
        let probe = WatchProbe::derive(&plain("build/compile_commands.json"));
        assert_eq!(
            probe.paths(),
            [PathBuf::from("build/compile_commands.json")]
        );
        assert!(probe.suffixes().is_empty());
    }

    #[test]
    fn wildcarded_name_plans_nothing() {
        // Serving `**/*.rs` supplementally would mean a second de-filtered full
        // walk of the root — the explicit cost bound. The main walk keeps it.
        assert!(WatchProbe::derive(&plain("**/*.rs")).is_empty());
        assert!(WatchProbe::derive(&plain("**/*")).is_empty());
        assert!(WatchProbe::derive(&plain("src/*.json")).is_empty());
        assert!(WatchProbe::derive(&plain("**/build/**/out.json")).is_empty());
    }

    #[test]
    fn absolute_literal_pattern_plans_an_absolute_path() {
        // rust-analyzer's out-of-root config watcher. The planner records it
        // verbatim; the in-root guard at probe time is what drops it.
        let probe = WatchProbe::derive(&plain("/home/u/.config/rust-analyzer"));
        assert_eq!(
            probe.paths(),
            [PathBuf::from("/home/u/.config/rust-analyzer")]
        );
    }

    #[test]
    fn base_anchored_literal_plans_a_direct_stat() {
        let probe = WatchProbe::derive(&relative("/w/target/out", "generated.rs"));
        assert_eq!(probe.paths(), [PathBuf::from("/w/target/out/generated.rs")]);
        assert!(probe.dirs().is_empty());
    }

    #[test]
    fn base_anchored_wildcard_plans_a_bounded_dir_walk() {
        // rust-analyzer's OUT_DIR watcher: recursion is genuinely needed, and
        // the server-named base is the bound.
        let probe = WatchProbe::derive(&relative("/w/target/out", "**/*"));
        assert_eq!(probe.dirs(), [PathBuf::from("/w/target/out")]);
        assert!(probe.paths().is_empty());
    }

    #[test]
    fn base_anchored_unanalyzable_pattern_falls_back_to_the_base_walk() {
        // A character class defeats the expander; the base still bounds it.
        let probe = WatchProbe::derive(&relative("/w/target/out", "gen-[0-9].rs"));
        assert_eq!(probe.dirs(), [PathBuf::from("/w/target/out")]);
    }

    #[test]
    fn base_anchored_alternatives_dedupe_the_walk() {
        let probe = WatchProbe::derive(&relative("/w/out", "{**/*.h,**/*.c}"));
        assert_eq!(probe.dirs(), [PathBuf::from("/w/out")]);
    }
}
