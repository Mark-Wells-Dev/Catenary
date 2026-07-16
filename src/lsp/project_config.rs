// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The `SessionStart` project-config setup nudge (misc 202).
//!
//! The language server reads its per-project settings *from the project* — its
//! own config-file convention (rust-analyzer → `rust-analyzer.toml`) alongside
//! `Cargo.toml`. When a served root routes to such a server and that file is
//! **absent**, the editor/receipt lint+feature surface may silently disagree with
//! the build (a `[clean]` receipt preceding a CI red, or an `E0432` false red
//! against a feature-gated module). The `SessionStart` hook surfaces a one-line
//! pointer so the agent knows to add the file — once per root per daemon instance,
//! never a nag.
//!
//! This module is the **pure core**: given a served root and the resolved config,
//! [`nudge_line`] decides whether a nudge is owed and renders its text. The
//! once-per-root dedup and the hook wiring live daemon-side
//! ([`crate::router`]); this seam is filesystem-in, `Option<String>`-out, so it
//! tests without a daemon.
//!
//! It is **data-driven** off two existing data sources, never a hand-coded server
//! list: a language's `root_markers` (the project signature — `Cargo.toml` marks a
//! Rust project) bind the root to a language, and that language's server bindings
//! carry the [`crate::recipes::ProjectConfigConvention`] via
//! [`crate::lsp::server_behavior::ServerProfile`]. A server with no convention
//! never nudges; a language with no marker present at the root never nudges.

use std::path::Path;

use crate::config::Config;
use crate::lsp::glob::is_glob_pattern;
use crate::lsp::server_behavior::ServerProfile;

/// The project-config setup nudge owed for `root`, or `None` when none is (misc
/// 202).
///
/// A nudge is owed when, for some configured language:
///
/// 1. one of the language's `root_markers` is present at `root` — the project
///    signature that says "this root is a `<lang>` project" (a Rust project has a
///    `Cargo.toml`);
/// 2. one of that language's bound servers carries a project-config convention
///    ([`ServerProfile::project_config`]); and
/// 3. the convention's file is **absent** at `root`.
///
/// The first language/server pair satisfying all three renders the line; the scan
/// is deterministic (languages sorted by key) so the same root always yields the
/// same line. Returns `None` the moment any leg fails for every candidate — no
/// marker present, no convention, or the file already there.
///
/// Pure but for the filesystem reads at `root` (the marker `exists()` checks and
/// the convention-file `exists()` check); it makes no network or daemon calls and
/// spawns nothing.
#[must_use]
pub fn nudge_line(root: &Path, config: &Config) -> Option<String> {
    // Deterministic order: the scan visits languages by key, so a root that could
    // match several never flickers between lines across runs.
    let mut languages: Vec<(&String, &crate::config::LanguageConfig)> =
        config.language.iter().collect();
    languages.sort_by(|a, b| a.0.cmp(b.0));

    for (_lang, lang_config) in languages {
        // Leg 1: is this root a project of this language? A marker must be present.
        let Some((markers, compiled)) = lang_config.marker_set() else {
            continue;
        };
        if !dir_has_marker(root, markers, compiled) {
            continue;
        }

        // Legs 2 + 3: does a bound server carry a convention whose file is absent?
        for binding in lang_config.servers() {
            let profile = ServerProfile::for_server(&binding.name);
            let Some(convention) = profile.project_config() else {
                continue;
            };
            if root.join(&convention.file).exists() {
                // The project already reads the file — nothing to nudge.
                continue;
            }
            return Some(render(&binding.name, convention));
        }
    }
    None
}

/// Render the one-line setup pointer for `server` with the given `convention`.
///
/// Names the server, its config file, that this root has none, and the practical
/// consequence, then points at the docs when the convention carries a pointer.
/// Kept to a single line — a passive pointer, not a wall.
fn render(server: &str, convention: &crate::recipes::ProjectConfigConvention) -> String {
    let base = format!(
        "{server} reads {file}; this root has none — lint/feature settings may not match your build.",
        file = convention.file,
    );
    match &convention.docs {
        Some(docs) => format!("{base} See {docs}"),
        None => base,
    }
}

/// Whether `dir` contains any of `markers` (exact filenames, fast `exists()`) or
/// matches any `compiled` glob marker (a `readdir` pass).
///
/// Mirrors the sub-root resolution predicate ([`crate::lsp::manager`]'s
/// `dir_has_marker`): the exact-vs-glob split matches
/// [`crate::config::language::LanguageConfig::marker_set`], so the nudge reads the
/// same project boundary the server instances do. Shared with the auto-install
/// detection ([`crate::auto_install`], lsm 05), which binds roots to languages
/// through the same predicate.
pub(crate) fn dir_has_marker(
    dir: &Path,
    markers: &[String],
    compiled: &[crate::lsp::glob::LspGlob],
) -> bool {
    for m in markers {
        if !is_glob_pattern(m) && dir.join(m).exists() {
            return true;
        }
    }
    if !compiled.is_empty()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_path = Path::new(&name);
            if compiled.iter().any(|g| g.is_match(name_path)) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// The shipped defaults ship a Rust language with `root_markers =
    /// ["Cargo.toml"]` bound to rust-analyzer, and rust-analyzer's manifest row
    /// carries the `rust-analyzer.toml` convention — so an empty config already
    /// exercises the whole nudge against the seeded data.
    fn default_config() -> Config {
        Config::load_from_sources(&[]).expect("default config loads")
    }

    #[test]
    fn nudges_once_when_convention_file_absent_at_a_marked_root() {
        // A Rust project (Cargo.toml present) with no rust-analyzer.toml: the
        // pointer is owed.
        let root = TempDir::new().expect("tempdir");
        fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("write Cargo.toml");

        let line = nudge_line(root.path(), &default_config())
            .expect("a Rust root without rust-analyzer.toml is nudged");
        assert!(
            line.contains("rust-analyzer.toml"),
            "the line names the convention file: {line}",
        );
        assert!(
            line.contains("rust-analyzer"),
            "the line names the server: {line}",
        );
        assert!(
            line.contains("may not match your build"),
            "the line states the consequence: {line}",
        );
        // The convention carries a docs pointer, so the line ends with it.
        assert!(line.contains("See "), "the line points at the docs: {line}");
    }

    #[test]
    fn silent_when_the_convention_file_is_present() {
        // Same Rust project, but the file exists — nothing to nudge.
        let root = TempDir::new().expect("tempdir");
        fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("write Cargo.toml");
        fs::write(
            root.path().join("rust-analyzer.toml"),
            "check.command = \"clippy\"\n",
        )
        .expect("write rust-analyzer.toml");

        assert_eq!(
            nudge_line(root.path(), &default_config()),
            None,
            "a root that already has the convention file is silent",
        );
    }

    #[test]
    fn silent_when_no_project_marker_is_present() {
        // No Cargo.toml (no project signature): the root is not a Rust project, so
        // the Rust convention does not apply and nothing else nudges.
        let root = TempDir::new().expect("tempdir");
        fs::write(root.path().join("notes.txt"), "hello\n").expect("write file");

        assert_eq!(
            nudge_line(root.path(), &default_config()),
            None,
            "a root with no language marker is never nudged",
        );
    }

    #[test]
    fn silent_when_the_bound_server_carries_no_convention() {
        // Go has a marker (go.mod) and a server (gopls), but gopls carries NO
        // project-config convention — so a Go project is silent. This pins that the
        // nudge is driven by the convention DATA, not by "any served root".
        let root = TempDir::new().expect("tempdir");
        fs::write(root.path().join("go.mod"), "module x\n").expect("write go.mod");

        assert_eq!(
            nudge_line(root.path(), &default_config()),
            None,
            "a language whose server has no convention is never nudged",
        );
    }

    #[test]
    fn render_omits_the_see_tail_without_docs() {
        let convention = crate::recipes::ProjectConfigConvention {
            file: "example.toml".to_string(),
            docs: None,
        };
        let line = render("example-ls", &convention);
        assert!(line.contains("example.toml"));
        assert!(
            !line.contains("See "),
            "a convention with no docs pointer renders no See tail: {line}",
        );
    }
}
