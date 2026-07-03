// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server definitions — how to run and configure a language server.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lsp::glob::LspGlob;

/// Server definition — how to run and configure a language server.
///
/// Defined in `[lsp.server.*]` config sections, referenced by name from
/// `[lsp.language.*]` entries. This is adapter-level config consumed by
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

    /// Diagnostic trust weight for the source this server emits natively
    /// (the source named after the definition), and the fallback for any
    /// sub-source not listed in [`sources`](Self::sources) (linters ticket 05).
    ///
    /// Higher = more trusted. Drives the cross-feeder dedup keeper and the
    /// provisional challenge. Absent ⇒ inherit the seeded default
    /// ([`DiagnosticWeights::rust_analyzer_default`](crate::config::DiagnosticWeights::rust_analyzer_default))
    /// or the [`BASELINE_WEIGHT`](crate::config::BASELINE_WEIGHT) for an
    /// otherwise-unlisted source.
    #[serde(default)]
    pub weight: Option<u32>,

    /// Per-sub-source weight overrides for a multi-source server
    /// (`[lsp.server.<name>.sources]`, linters ticket 05).
    ///
    /// rust-analyzer emits three sources — `rust-analyzer` (native), `rustc`,
    /// and `clippy` (flycheck) — that need different weights. The native source
    /// inherits [`weight`](Self::weight); each entry here overrides one
    /// sub-source by its emitted `source` name.
    #[serde(default)]
    pub sources: HashMap<String, u32>,

    /// Regex marking the native source's *provisional* diagnostic code band
    /// (linters ticket 05).
    ///
    /// A native finding whose `code` matches survives only if corroborated by a
    /// heavier source or unchallenged (no strictly-heavier source reported for
    /// the file). For rust-analyzer the band is the rustc error-code namespace
    /// (`^E[0-9]+$`): a native `E####` flycheck does not corroborate is a phantom
    /// (misc 115). Validated at config load; compiled lazily when weights are
    /// resolved.
    #[serde(default)]
    pub provisional: Option<String>,
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
