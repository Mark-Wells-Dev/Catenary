// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server definitions — how to run and configure a language server.

use std::collections::HashMap;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::lsp::glob::LspGlob;

/// Server definition — how to run and configure a language server.
///
/// Defined in `[server.*]` config sections, referenced by name from
/// `[language.*]` entries. This is adapter-level config consumed by
/// the LSP client layer — the routing core never sees it directly.
///
/// **Sync note:** Config-visible fields must be listed in
/// [`super::parse::SERVER_DEF_KEYS`] for misplaced-field detection.
/// `test_server_def_keys_sync` enforces this.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct ServerDef {
    /// The command to execute (e.g., "rust-analyzer", "clangd").
    #[serde(default)]
    pub command: String,

    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set on the spawned server process.
    ///
    /// Variables are **added** to the inherited environment. If a key
    /// already exists, the config value wins.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,

    /// Initialization options to pass to the LSP server.
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,

    /// Server-specific settings returned in `workspace/configuration`
    /// responses.
    #[serde(default)]
    pub settings: Option<serde_json::Value>,

    /// Minimum diagnostic severity to deliver to agents.
    /// Valid values: `"error"`, `"warning"`, `"information"`, `"hint"`.
    /// When absent, all severities are delivered.
    #[serde(default)]
    pub min_severity: Option<String>,

    /// Whether this server supports single-file mode (tier 3).
    ///
    /// When `true`, the server may be spawned with `rootUri: null` and
    /// `workspaceFolders: null` for files outside all workspace roots.
    /// Servers like `bash-language-server` work well without a project
    /// root; servers like `rust-analyzer` require one and should leave
    /// this `false` (the default).
    #[serde(default)]
    pub single_file: bool,

    /// Glob patterns to filter which files this server handles
    /// within its language. Matched against the filename (not path).
    /// Servers without `file_patterns` handle all files for their
    /// language.
    /// Example: `["PKGBUILD", "*.ebuild"]`
    #[serde(default)]
    pub file_patterns: Vec<String>,

    /// Compiled glob patterns from `file_patterns`. Populated by
    /// [`Self::compile_patterns`] after deserialization.
    #[serde(skip)]
    pub compiled_patterns: Vec<LspGlob>,
}

/// Per-root, cross-feeder diagnostic source-precedence policy (misc 115, bug
/// 42; hoisted to per-root in workstream 34 ticket 02).
///
/// A **generic, feeder-agnostic** reconciliation keyed on the standard LSP
/// `Diagnostic.source` field. Splits the sources that report for a file into
/// two roles:
///
/// - **advisory** — fast but unreliable in some band (e.g. rust-analyzer's
///   in-memory native HIR/macro analysis).
/// - **authoritative** — ground truth in that band (e.g. rust-analyzer's
///   flycheck = the real `rustc`/`clippy`).
///
/// The rule applied over a file's merged diagnostic set (all feeders — every
/// language server **and** every linter): an advisory source's diagnostics are
/// dropped in the band an authoritative source owns, **once the authoritative
/// source has reported for that file**. Scoping the rule to a
/// [`code_pattern`](Self::code_pattern) keeps advisory diagnostics that fall
/// *outside* the authoritative band (an advisory source's own lints, an
/// unresolved-import preview) — they only lose trust inside the band.
///
/// Precedence is configured **per root** (top-level `[[diagnostic_precedence]]`
/// in user config or `.catenary.toml`), not per-`[server.*]`. It is deliberately
/// narrow — the rust-analyzer / flycheck ground-truth tool — **not** a lever for
/// ranking linters against language servers; that overlap is handled by
/// opinion-free dedup, not precedence.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct DiagnosticPrecedence {
    /// Source names whose diagnostics are advisory in the band (dropped when
    /// the authoritative source has reported, see [`Self::code_pattern`]).
    #[serde(default)]
    pub advisory_sources: Vec<String>,

    /// Source names whose diagnostics are authoritative — ground truth in the
    /// band. Their presence (for a file) clobbers advisory diagnostics there.
    #[serde(default)]
    pub authoritative_sources: Vec<String>,

    /// Regex scoping the reconciliation to a code band, matched against the
    /// diagnostic's `code` (rendered as a string). When set, only advisory
    /// diagnostics whose code matches are eligible to be dropped; advisory
    /// diagnostics outside the band are always kept.
    ///
    /// For rust-analyzer the band is the rustc error-code namespace
    /// (`^E[0-9]+$`): a rustc error code is by definition something `rustc`
    /// produces, so a native `E####` that flycheck does not corroborate is a
    /// false positive. When absent, the whole-diagnostic set is the band.
    #[serde(default)]
    pub code_pattern: Option<String>,

    /// Compiled form of [`Self::code_pattern`]. Populated by
    /// [`Self::compile`] after deserialization.
    #[serde(skip)]
    pub compiled_code_pattern: Option<Regex>,
}

impl DiagnosticPrecedence {
    /// Returns `true` if `source` is listed as authoritative.
    #[must_use]
    pub fn is_authoritative(&self, source: &str) -> bool {
        self.authoritative_sources.iter().any(|s| s == source)
    }

    /// Returns `true` if `source` is listed as advisory.
    #[must_use]
    pub fn is_advisory(&self, source: &str) -> bool {
        self.advisory_sources.iter().any(|s| s == source)
    }

    /// Returns `true` if `code` falls inside the authoritative band.
    ///
    /// An absent or empty [`code_pattern`](Self::code_pattern) means the band
    /// is unrestricted — every code is in-band.
    #[must_use]
    pub fn code_in_band(&self, code: &str) -> bool {
        self.compiled_code_pattern
            .as_ref()
            .is_none_or(|re| re.is_match(code))
    }

    /// Compiles [`Self::code_pattern`] into [`Self::compiled_code_pattern`].
    ///
    /// Called once after deserialization. Validation already checks the
    /// pattern compiles, so this is normally infallible at load time.
    ///
    /// # Errors
    ///
    /// Returns an error if `code_pattern` is not a valid regex.
    pub fn compile(&mut self) -> Result<()> {
        self.compiled_code_pattern = match &self.code_pattern {
            Some(pat) => Some(
                Regex::new(pat)
                    .with_context(|| format!("diagnostic_precedence code_pattern '{pat}'"))?,
            ),
            None => None,
        };
        Ok(())
    }

    /// The shipped default precedence policy: rust-analyzer's native analysis is
    /// advisory against its flycheck (`rustc`/`clippy`) ground truth, scoped to
    /// the rustc `E####` error-code band (misc 115, bug 42).
    ///
    /// Returned pre-compiled so it works on the [`Config::default`] path that
    /// skips the post-load compile step. Keyed purely on `source` names, so it is
    /// a no-op for any root whose diagnostics carry none of those sources — which
    /// is why it can ship as a single global default rather than a per-server
    /// row.
    ///
    /// [`Config::default`]: crate::config::Config::default
    #[must_use]
    pub fn rust_analyzer_default() -> Self {
        let pattern = "^E[0-9]+$";
        // The pattern is a compile-time constant known to be valid; on the
        // impossible regex error, fall back to an inert (empty-source) policy
        // rather than an unrestricted band — an unrestricted band would drop
        // *all* advisory diagnostics, far worse than doing nothing.
        Regex::new(pattern).map_or_else(
            |_| Self::default(),
            |re| Self {
                advisory_sources: vec!["rust-analyzer".to_string()],
                authoritative_sources: vec!["rustc".to_string(), "clippy".to_string()],
                code_pattern: Some(pattern.to_string()),
                compiled_code_pattern: Some(re),
            },
        )
    }
}

impl ServerDef {
    /// Compiles `file_patterns` into [`LspGlob`] matchers.
    ///
    /// Called once after deserialization. Fails fast on invalid patterns
    /// so `catenary doctor` can surface the issue at config load time.
    ///
    /// # Errors
    ///
    /// Returns an error if any pattern in `file_patterns` fails to compile.
    pub fn compile_patterns(&mut self) -> Result<()> {
        self.compiled_patterns = self
            .file_patterns
            .iter()
            .map(|p| LspGlob::new(p).with_context(|| format!("file_patterns glob '{p}'")))
            .collect::<Result<Vec<_>>>()?;
        Ok(())
    }
}
