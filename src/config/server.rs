// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server definitions — how to run and configure a language server.

use std::collections::HashMap;

use anyhow::{Context, Result};
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
