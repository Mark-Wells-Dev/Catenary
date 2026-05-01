// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Language configuration.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::{self, Deserializer};

use crate::lsp::glob::{LspGlob, is_glob_pattern};

/// LSP methods dispatched through [`crate::lsp::LspClientManager::get_servers`].
///
/// Each variant corresponds to a capability gate on the LSP server and
/// an LSP protocol method name. Used as validated values in
/// [`ServerBinding::disabled_methods`] and as the type-safe `method`
/// parameter in dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DispatchMethod {
    /// `textDocument/references`
    References,
    /// `textDocument/documentSymbol`
    DocumentSymbol,
    /// `textDocument/rename`
    Rename,
    /// `textDocument/implementation`
    Implementation,
    /// `textDocument/prepareCallHierarchy`
    CallHierarchy,
    /// `textDocument/prepareTypeHierarchy`
    TypeHierarchy,
}

impl DispatchMethod {
    /// All valid method names, for error messages.
    const ALL_NAMES: &[&str] = &[
        "textDocument/references",
        "textDocument/documentSymbol",
        "textDocument/rename",
        "textDocument/implementation",
        "textDocument/prepareCallHierarchy",
        "textDocument/prepareTypeHierarchy",
    ];

    /// Returns the LSP protocol method name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::References => "textDocument/references",
            Self::DocumentSymbol => "textDocument/documentSymbol",
            Self::Rename => "textDocument/rename",
            Self::Implementation => "textDocument/implementation",
            Self::CallHierarchy => "textDocument/prepareCallHierarchy",
            Self::TypeHierarchy => "textDocument/prepareTypeHierarchy",
        }
    }
}

impl<'de> Deserialize<'de> for DispatchMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MethodVisitor;

        impl de::Visitor<'_> for MethodVisitor {
            type Value = DispatchMethod;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an LSP method name like \"textDocument/references\"")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "textDocument/references" => Ok(DispatchMethod::References),
                    "textDocument/documentSymbol" => Ok(DispatchMethod::DocumentSymbol),
                    "textDocument/rename" => Ok(DispatchMethod::Rename),
                    "textDocument/implementation" => Ok(DispatchMethod::Implementation),
                    "textDocument/prepareCallHierarchy" => Ok(DispatchMethod::CallHierarchy),
                    "textDocument/prepareTypeHierarchy" => Ok(DispatchMethod::TypeHierarchy),
                    "textDocument/diagnostic" => Err(de::Error::custom(
                        "textDocument/diagnostic cannot be suppressed via disabled_methods; \
                         use `diagnostics = false` on the binding instead",
                    )),
                    _ => Err(de::Error::unknown_variant(v, DispatchMethod::ALL_NAMES)),
                }
            }
        }

        deserializer.deserialize_str(MethodVisitor)
    }
}

/// A server reference within a language binding.
///
/// Supports both bare string form (`"foo"`) and inline-table form
/// (`{ name = "foo", diagnostics = false }`). Bare strings expand
/// to `{ name, diagnostics: true, disabled_methods: [] }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerBinding {
    /// Server name (references a `[server.*]` entry).
    pub name: String,

    /// Whether this server delivers diagnostics for this language.
    /// Defaults to `true`.
    pub diagnostics: bool,

    /// LSP methods suppressed for this binding.
    ///
    /// When a method appears in this list, the server is excluded from
    /// dispatch for that method under this language binding.
    pub disabled_methods: Vec<DispatchMethod>,
}

impl ServerBinding {
    /// Creates a new binding with diagnostics enabled and no disabled
    /// methods (the defaults).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            diagnostics: true,
            disabled_methods: Vec::new(),
        }
    }

    /// Returns `true` if the given dispatch method is suppressed for
    /// this binding.
    #[must_use]
    pub fn is_method_disabled(&self, method: DispatchMethod) -> bool {
        self.disabled_methods.contains(&method)
    }
}

impl<'de> Deserialize<'de> for ServerBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ServerBindingVisitor;

        impl<'de> de::Visitor<'de> for ServerBindingVisitor {
            type Value = ServerBinding;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "a server name string or inline table \
                     { name = \"...\", diagnostics = ... }",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ServerBinding {
                    name: v.to_string(),
                    diagnostics: true,
                    disabled_methods: Vec::new(),
                })
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut name: Option<String> = None;
                let mut diagnostics: Option<bool> = None;
                let mut disabled_methods: Option<Vec<DispatchMethod>> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            if name.is_some() {
                                return Err(de::Error::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        "diagnostics" => {
                            if diagnostics.is_some() {
                                return Err(de::Error::duplicate_field("diagnostics"));
                            }
                            diagnostics = Some(map.next_value()?);
                        }
                        "disabled_methods" => {
                            if disabled_methods.is_some() {
                                return Err(de::Error::duplicate_field("disabled_methods"));
                            }
                            disabled_methods = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["name", "diagnostics", "disabled_methods"],
                            ));
                        }
                    }
                }

                let name = name.ok_or_else(|| de::Error::missing_field("name"))?;
                Ok(ServerBinding {
                    name,
                    diagnostics: diagnostics.unwrap_or(true),
                    disabled_methods: disabled_methods.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_any(ServerBindingVisitor)
    }
}

/// Per-language configuration for how Catenary handles a language.
///
/// Each entry references one or more server definitions from `[server.*]`
/// via the `servers` list and controls diagnostic severity filtering.
/// Classification fields (`extensions`, `filenames`, `shebangs`) define
/// how files are mapped to this language.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LanguageConfig {
    /// Ordered list of server bindings (references `[server.*]` entries).
    /// Order defines dispatch priority.
    pub servers: Vec<ServerBinding>,

    /// Whether to deliver diagnostics for this language.
    /// Defaults to `true`. AND with per-binding `diagnostics`
    /// to determine effective delivery per server.
    pub diagnostics: bool,

    /// File extensions (without dot) that classify as this language.
    /// Example: `["sh", "bash", "zsh"]`
    #[serde(default)]
    pub extensions: Option<Vec<String>>,

    /// Exact filenames that classify as this language.
    /// Example: `["PKGBUILD", "Makefile"]`
    #[serde(default)]
    pub filenames: Option<Vec<String>>,

    /// Interpreter basenames for shebang detection.
    /// Matches `#!/bin/X` and `#!/usr/bin/env X`.
    /// Example: `["bash", "sh", "zsh"]`
    #[serde(default)]
    pub shebangs: Option<Vec<String>>,

    /// Root marker filenames or glob patterns for sub-root resolution.
    ///
    /// When set, Catenary walks up from each file toward the workspace
    /// root boundary, stopping at the first directory containing any
    /// marker. That directory becomes the server instance's root.
    /// Different resolved roots produce different server instances.
    ///
    /// Entries without glob metacharacters (`*`, `?`, `[`) are treated
    /// as exact filenames (fast `exists()` check). Entries with glob
    /// metacharacters are compiled and matched against directory entries.
    ///
    /// Markers are a property of the language ecosystem — `Cargo.toml`
    /// defines a Rust project boundary regardless of which server is used.
    /// Defaults are shipped in `defaults/languages.toml` for common
    /// languages. Override with explicit `root_markers = [...]` or disable
    /// with `root_markers = []`.
    #[serde(default)]
    pub root_markers: Option<Vec<String>>,

    /// Compiled glob patterns from `root_markers`. Populated by
    /// [`Self::compile_markers`] after deserialization. Contains only
    /// the entries that have glob metacharacters.
    #[serde(skip)]
    pub compiled_markers: Vec<LspGlob>,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            diagnostics: true,
            extensions: None,
            filenames: None,
            shebangs: None,
            root_markers: None,
            compiled_markers: Vec::new(),
        }
    }
}

impl LanguageConfig {
    /// Merges another config layer into this one (field-level).
    ///
    /// - `servers`: non-empty overlay replaces, empty preserves.
    /// - `diagnostics`: overlay always replaces (cannot distinguish
    ///   absent from default in serde without `Option`).
    /// - `extensions`/`filenames`/`shebangs`: `Some` replaces, `None` preserves.
    pub fn merge(&mut self, other: Self) {
        if !other.servers.is_empty() {
            self.servers = other.servers;
        }
        self.diagnostics = other.diagnostics;
        if other.extensions.is_some() {
            self.extensions = other.extensions;
        }
        if other.filenames.is_some() {
            self.filenames = other.filenames;
        }
        if other.shebangs.is_some() {
            self.shebangs = other.shebangs;
        }
        if other.root_markers.is_some() {
            self.root_markers = other.root_markers;
        }
    }

    /// Returns the active root markers, if any.
    ///
    /// Returns `Some(&[String])` when markers are configured (either
    /// from defaults or user-specified). Returns `None` when markers
    /// are absent or explicitly disabled (`root_markers = []`).
    #[must_use]
    pub fn active_markers(&self) -> Option<&[String]> {
        self.root_markers
            .as_deref()
            .filter(|markers| !markers.is_empty())
    }

    /// Returns the active root markers split into exact filenames and
    /// compiled glob patterns.
    ///
    /// Returns `None` when markers are absent or disabled (`root_markers = []`).
    #[must_use]
    pub fn marker_set(&self) -> Option<(&[String], &[LspGlob])> {
        self.active_markers()
            .map(|m| (m, self.compiled_markers.as_slice()))
    }

    /// Compiles glob patterns in `root_markers` into [`LspGlob`] matchers.
    ///
    /// Called once after deserialization. Only entries containing glob
    /// metacharacters (`*`, `?`, `[`) are compiled — exact filenames
    /// use the fast `exists()` path and don't need compilation.
    ///
    /// # Errors
    ///
    /// Returns an error if any glob pattern in `root_markers` fails to compile.
    pub fn compile_markers(&mut self) -> Result<()> {
        if let Some(markers) = &self.root_markers {
            self.compiled_markers = markers
                .iter()
                .filter(|m| is_glob_pattern(m))
                .map(|m| LspGlob::new(m).with_context(|| format!("root_markers glob '{m}'")))
                .collect::<Result<Vec<_>>>()?;
        }
        Ok(())
    }

    /// Returns `true` if this entry has any classification fields set.
    #[must_use]
    pub const fn has_classification(&self) -> bool {
        self.extensions.is_some() || self.filenames.is_some() || self.shebangs.is_some()
    }
}

impl LanguageConfig {
    /// Whether diagnostics from `server_name` should be delivered
    /// for this language binding.
    ///
    /// Returns `false` if the server is not in the bindings list or
    /// if either the language-level or per-binding `diagnostics` flag
    /// is `false`.
    #[must_use]
    pub fn diagnostics_enabled(&self, server_name: &str) -> bool {
        self.diagnostics
            && self
                .servers
                .iter()
                .find(|b| b.name == server_name)
                .is_some_and(|b| b.diagnostics)
    }
}
