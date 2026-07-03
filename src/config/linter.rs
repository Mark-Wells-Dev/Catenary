// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Linter definitions — standalone linters run as a second diagnostic feeder.
//!
//! A `[linter.rule.<name>]` section configures a standalone linter (shellcheck,
//! actionlint, yamllint, or any SARIF-emitting tool) that Catenary runs over the
//! modified-file set during `catenary diagnostics`, translating its output into
//! LSP-shaped diagnostics that merge with the LSP feeder's. The blessed adapters
//! (keyed by linter name) and the generic SARIF adapter live in
//! [`crate::bridge::linter`]; this module owns only the config shape + routing
//! glob compilation.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::lsp::glob::LspGlob;

/// A single `[linter.rule.<name>]` configuration entry.
///
/// Lives on both the user [`Config`](super::Config) and the per-root
/// [`ProjectConfig`](super::ProjectConfig). The effective set for a root is the
/// user set unioned with that root's project set, the project winning on a name
/// collision (so a project can override or [`disable`](Self::disable) a
/// user-configured linter by name).
///
/// `patterns` are **root-relative path globs** (e.g.
/// `.github/workflows/*.{yml,yaml}`), not filename globs — an unanchored
/// `*.yaml` would fire on every YAML in the tree.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinterConfig {
    /// The executable to run (e.g. `"shellcheck"`).
    pub command: String,
    /// Arguments passed before the file paths (e.g. `["-f", "json1"]`).
    pub args: Vec<String>,
    /// Root-relative path globs selecting which files this linter handles.
    pub patterns: Vec<String>,
    /// Disables this linter for the root it resolves under (default `false`).
    ///
    /// A project entry can disable a user-configured linter by setting this on
    /// an entry with the same name.
    pub disable: bool,
    /// Diagnostic trust weight for this linter's source (linters ticket 05).
    ///
    /// A linter is a 1:1 emitter — the diagnostic `source` equals the linter
    /// name — so this single weight covers it. Higher = more trusted. Absent ⇒
    /// the [`BASELINE_WEIGHT`](super::BASELINE_WEIGHT). Drives the cross-feeder
    /// dedup keeper and the provisional challenge.
    pub weight: Option<u32>,
    /// Compiled form of [`Self::patterns`]. Populated by
    /// [`Self::compile_patterns`] after deserialization.
    #[serde(skip)]
    pub compiled_patterns: Vec<LspGlob>,
}

impl LinterConfig {
    /// Builds a compiled linter config from its parts.
    ///
    /// Used by tests and the inherit-when-absent defaults path. Production
    /// config travels through deserialization + [`Self::compile_patterns`].
    ///
    /// # Errors
    ///
    /// Returns an error if any pattern in `patterns` is not a valid glob.
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        patterns: Vec<String>,
    ) -> Result<Self> {
        let mut linter = Self {
            command: command.into(),
            args,
            patterns,
            disable: false,
            weight: None,
            compiled_patterns: Vec::new(),
        };
        linter.compile_patterns()?;
        Ok(linter)
    }

    /// Compiles [`Self::patterns`] into [`LspGlob`] matchers.
    ///
    /// Called once after deserialization, mirroring
    /// [`ServerDef::compile_patterns`](super::ServerDef::compile_patterns).
    /// Fails fast on an invalid pattern so `catenary doctor` surfaces the issue
    /// at config-load time.
    ///
    /// # Errors
    ///
    /// Returns an error if any pattern in `patterns` fails to compile.
    pub fn compile_patterns(&mut self) -> Result<()> {
        self.compiled_patterns = self
            .patterns
            .iter()
            .map(|p| LspGlob::new(p).with_context(|| format!("linter patterns glob '{p}'")))
            .collect::<Result<Vec<_>>>()?;
        Ok(())
    }

    /// Whether the root-relative path matches any of this linter's patterns.
    ///
    /// Returns `false` when the linter has no patterns — an unrouted linter
    /// covers nothing (matching everything would fire it on every file).
    #[must_use]
    pub fn matches(&self, rel: &Path) -> bool {
        self.compiled_patterns.iter().any(|g| g.is_match(rel))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn matches_root_relative_path_glob() {
        let linter = LinterConfig::new(
            "actionlint",
            vec![],
            vec![".github/workflows/*.{yml,yaml}".to_string()],
        )
        .expect("compile");
        assert!(linter.matches(Path::new(".github/workflows/ci.yml")));
        assert!(linter.matches(Path::new(".github/workflows/cd.yaml")));
        // Not anchored under .github/workflows ⇒ no match.
        assert!(!linter.matches(Path::new("docs/ci.yml")));
        assert!(!linter.matches(Path::new("ci.yml")));
    }

    #[test]
    fn no_patterns_matches_nothing() {
        let linter = LinterConfig::new("shellcheck", vec![], vec![]).expect("compile");
        assert!(!linter.matches(Path::new("script.sh")));
    }

    #[test]
    fn double_star_crosses_directories() {
        let linter =
            LinterConfig::new("shellcheck", vec![], vec!["**/*.sh".to_string()]).expect("compile");
        assert!(linter.matches(Path::new("script.sh")));
        assert!(linter.matches(Path::new("scripts/deep/build.sh")));
        assert!(!linter.matches(Path::new("script.bash")));
    }

    #[test]
    fn invalid_glob_errors() {
        let err = LinterConfig::new("x", vec![], vec!["[unterminated".to_string()]);
        assert!(err.is_err(), "invalid glob must fail to compile");
    }
}
