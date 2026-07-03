// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Instance identity types for LSP server routing.
//!
//! `InstanceKey` uniquely identifies an LSP server instance across three
//! dimensions: language, server name, and routing scope. `Scope` determines
//! how an instance binds to workspace roots.

use std::fmt;
use std::path::{Path, PathBuf};

/// Routing scope for an LSP server instance.
///
/// Determines how the instance is bound to workspace roots.
/// `Root` is the primary variant — every workspace root gets its
/// own server instance. `SingleFile` in misc 28b (tier 3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// One instance per root. Unrelated projects never share
    /// an LSP server — each root is an isolated fault domain.
    Root(PathBuf),
    /// Tier 3 — single-file mode (misc 28b).
    SingleFile,
}

impl Scope {
    /// Returns the scope kind as a machine-readable string.
    ///
    /// Used for DB writes, serialization, and structured logging.
    #[must_use]
    pub const fn kind_str(&self) -> &str {
        match self {
            Self::Root(_) => "root",
            Self::SingleFile => "single_file",
        }
    }

    /// Returns the root path for scopes that have one.
    ///
    /// `Root` carries a path; `SingleFile` does not.
    #[must_use]
    pub fn root_path(&self) -> Option<&Path> {
        match self {
            Self::Root(p) => Some(p),
            Self::SingleFile => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(p) => write!(f, "root({})", p.display()),
            Self::SingleFile => write!(f, "single_file"),
        }
    }
}

/// Unique identity for an LSP server instance.
///
/// Three components are needed for uniqueness:
/// - `language_id` alone fails with multiple servers per language.
/// - `server` alone fails when one server serves multiple languages.
/// - `scope` distinguishes per-root instances of the same server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceKey {
    /// LSP language identifier (from config key).
    pub language_id: String,
    /// Server name (references a `[lsp.server.*]` config entry).
    pub server: String,
    /// Routing scope.
    pub scope: Scope,
}

impl InstanceKey {
    /// Creates a new `InstanceKey`.
    #[must_use]
    pub const fn new(language_id: String, server: String, scope: Scope) -> Self {
        Self {
            language_id,
            server,
            scope,
        }
    }
}

impl fmt::Display for InstanceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.language_id, self.server, self.scope)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_instance_key_eq_hash() {
        let key1 = InstanceKey::new(
            "rust".to_string(),
            "rust-analyzer".to_string(),
            Scope::Root(PathBuf::from("/project")),
        );
        let key2 = InstanceKey::new(
            "rust".to_string(),
            "rust-analyzer".to_string(),
            Scope::Root(PathBuf::from("/project")),
        );
        assert_eq!(key1, key2);

        let mut set = HashSet::new();
        set.insert(key1.clone());
        assert!(set.contains(&key2));

        // Different language_id
        let key3 = InstanceKey::new(
            "python".to_string(),
            "rust-analyzer".to_string(),
            Scope::Root(PathBuf::from("/project")),
        );
        assert_ne!(key1, key3);
        assert!(!set.contains(&key3));

        // Different server
        let key4 = InstanceKey::new(
            "rust".to_string(),
            "other-server".to_string(),
            Scope::Root(PathBuf::from("/project")),
        );
        assert_ne!(key1, key4);

        // Root vs Root with different paths
        let key5 = InstanceKey::new(
            "rust".to_string(),
            "rust-analyzer".to_string(),
            Scope::Root(PathBuf::from("/other")),
        );
        assert_ne!(key1, key5);

        // SingleFile scope
        let key6 = InstanceKey::new(
            "rust".to_string(),
            "rust-analyzer".to_string(),
            Scope::SingleFile,
        );
        assert_ne!(key1, key6);
    }

    #[test]
    fn test_instance_key_display() {
        let key = InstanceKey::new(
            "python".to_string(),
            "pyright".to_string(),
            Scope::Root(PathBuf::from("/home/user/project")),
        );
        assert_eq!(key.to_string(), "python:pyright:root(/home/user/project)");

        let key = InstanceKey::new("text".to_string(), "ltex".to_string(), Scope::SingleFile);
        assert_eq!(key.to_string(), "text:ltex:single_file");
    }

    #[test]
    fn test_scope_kind_str() {
        assert_eq!(Scope::Root(PathBuf::from("/r")).kind_str(), "root");
        assert_eq!(Scope::SingleFile.kind_str(), "single_file");
    }

    #[test]
    fn test_scope_root_path() {
        let root = Scope::Root(PathBuf::from("/root"));
        assert_eq!(root.root_path(), Some(Path::new("/root")));

        assert_eq!(Scope::SingleFile.root_path(), None);
    }
}
