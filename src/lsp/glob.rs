// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! LSP file watcher glob patterns and change types.
//!
//! Provides [`LspGlob`] for compiling and matching LSP 3.17 glob patterns,
//! [`GlobPattern`] for handling both plain and `RelativePattern` forms,
//! and the associated change-event types ([`WatchKind`], [`FileChangeType`],
//! [`FileChange`]).

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use globset::{GlobBuilder, GlobMatcher};

/// Compiled LSP glob pattern.
///
/// Wraps a [`GlobMatcher`] compiled with `literal_separator(true)` so that
/// `*` does not cross path segment boundaries, matching LSP 3.17 semantics.
#[derive(Clone, Debug)]
pub struct LspGlob {
    matcher: GlobMatcher,
}

impl LspGlob {
    /// Compiles an LSP 3.17 glob pattern string.
    ///
    /// Uses `literal_separator(true)` so that `*` matches within a single
    /// path segment. `**` crosses segment boundaries as usual.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid glob.
    pub fn new(pattern: &str) -> Result<Self> {
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| anyhow!("invalid glob pattern: {e}"))?
            .compile_matcher();
        Ok(Self { matcher })
    }

    /// Tests whether a path matches this pattern.
    #[must_use]
    pub fn is_match(&self, path: &Path) -> bool {
        self.matcher.is_match(path)
    }
}

/// Returns `true` if the string contains glob metacharacters (`*`, `?`, `[`).
#[must_use]
pub fn is_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Parsed glob pattern — plain string or `RelativePattern`.
#[derive(Clone)]
pub enum GlobPattern {
    /// Plain glob — matched relative to workspace roots.
    Plain(LspGlob),
    /// Anchored to a base URI — strip base prefix, match remainder.
    Relative {
        /// Base directory (converted from `file://` URI).
        base: PathBuf,
        /// Compiled glob pattern.
        pattern: LspGlob,
    },
}

impl GlobPattern {
    /// Parses from the JSON `globPattern` field of a `FileSystemWatcher`.
    ///
    /// If the value is a string, it's a plain pattern.
    /// If it's an object with `baseUri` and `pattern`, it's a
    /// `RelativePattern`.
    ///
    /// # Errors
    ///
    /// Returns an error if the value is neither a string nor a valid
    /// `RelativePattern` object, or if the glob pattern fails to compile.
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        if let Some(s) = value.as_str() {
            return Ok(Self::Plain(LspGlob::new(s)?));
        }

        let obj = value
            .as_object()
            .ok_or_else(|| anyhow!("globPattern must be a string or object"))?;

        let pattern_str = obj
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("RelativePattern missing 'pattern' field"))?;

        let base_uri = obj
            .get("baseUri")
            .ok_or_else(|| anyhow!("RelativePattern missing 'baseUri' field"))?;

        // baseUri can be a URI string or a WorkspaceFolder { uri, name }.
        let uri_str = if let Some(s) = base_uri.as_str() {
            s
        } else {
            base_uri
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("baseUri must be a URI string or WorkspaceFolder object"))?
        };

        let base = uri_to_path(uri_str)?;
        let pattern = LspGlob::new(pattern_str)?;

        Ok(Self::Relative { base, pattern })
    }

    /// Tests whether an absolute path matches this pattern.
    ///
    /// For `Plain`, tries stripping each root as a prefix and matches the
    /// remainder. Returns `true` if any root produces a match.
    ///
    /// For `Relative`, strips `base` as the prefix and matches the remainder.
    #[must_use]
    pub fn is_match(&self, absolute_path: &Path, roots: &[PathBuf]) -> bool {
        match self {
            Self::Plain(glob) => roots.iter().any(|root| {
                absolute_path
                    .strip_prefix(root)
                    .is_ok_and(|rel| glob.is_match(rel))
            }),
            Self::Relative { base, pattern } => absolute_path
                .strip_prefix(base)
                .is_ok_and(|rel| pattern.is_match(rel)),
        }
    }
}

/// Converts a `file://` URI to a filesystem path.
fn uri_to_path(uri: &str) -> Result<PathBuf> {
    uri.strip_prefix("file://")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("expected file:// URI, got: {uri}"))
}

/// Bitmask for file watcher event kinds.
///
/// `Create = 1`, `Change = 2`, `Delete = 4`. Default `7` (all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchKind(u8);

impl WatchKind {
    /// Bit flag for file creation events.
    pub const CREATE: u8 = 1;
    /// Bit flag for file change events.
    pub const CHANGE: u8 = 2;
    /// Bit flag for file deletion events.
    pub const DELETE: u8 = 4;
    /// All event kinds (create + change + delete).
    pub const ALL: u8 = 7;

    /// Creates a `WatchKind` from an optional LSP value.
    ///
    /// Defaults to [`Self::ALL`] if `None` (per LSP spec).
    #[must_use]
    pub fn from_value(value: Option<u8>) -> Self {
        Self(value.unwrap_or(Self::ALL))
    }

    /// Tests whether this watch kind includes the given change type.
    #[must_use]
    pub const fn matches(self, change_type: FileChangeType) -> bool {
        let bit = match change_type {
            FileChangeType::Created => Self::CREATE,
            FileChangeType::Changed => Self::CHANGE,
            FileChangeType::Deleted => Self::DELETE,
        };
        self.0 & bit != 0
    }
}

/// LSP `FileChangeType` — the kind of filesystem change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileChangeType {
    /// File was created.
    Created = 1,
    /// File was changed.
    Changed = 2,
    /// File was deleted.
    Deleted = 3,
}

/// A filesystem change event.
#[derive(Debug)]
pub struct FileChange {
    /// Absolute path of the changed file or directory.
    pub path: PathBuf,
    /// The type of change.
    pub change_type: FileChangeType,
}

/// A set of file watchers from a single `client/registerCapability` registration.
pub struct FileWatcherRegistration {
    /// The parsed watchers in this registration.
    pub watchers: Vec<ParsedWatcher>,
}

/// A single file watcher: a glob pattern and the event kinds to watch for.
pub struct ParsedWatcher {
    /// The compiled glob pattern.
    pub pattern: GlobPattern,
    /// Which event kinds this watcher is interested in.
    pub kind: WatchKind,
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::path::Path;

    // ── LspGlob ──────────────────────────────────────────────────

    #[test]
    fn lsp_glob_star_matches_single_segment() {
        let glob = LspGlob::new("*.rs").expect("valid glob");
        assert!(glob.is_match(Path::new("foo.rs")));
        assert!(!glob.is_match(Path::new("a/foo.rs")));
    }

    #[test]
    fn lsp_glob_double_star_matches_multiple_segments() {
        let glob = LspGlob::new("**/*.rs").expect("valid glob");
        assert!(glob.is_match(Path::new("foo.rs")));
        assert!(glob.is_match(Path::new("a/foo.rs")));
        assert!(glob.is_match(Path::new("a/b/foo.rs")));
    }

    #[test]
    fn lsp_glob_question_mark() {
        let glob = LspGlob::new("?.rs").expect("valid glob");
        assert!(glob.is_match(Path::new("a.rs")));
        assert!(!glob.is_match(Path::new("ab.rs")));
    }

    #[test]
    fn lsp_glob_alternation() {
        let glob = LspGlob::new("**/*.{ts,js}").expect("valid glob");
        assert!(glob.is_match(Path::new("foo.ts")));
        assert!(glob.is_match(Path::new("bar.js")));
    }

    #[test]
    fn lsp_glob_character_class() {
        let glob = LspGlob::new("example.[0-9]").expect("valid glob");
        assert!(glob.is_match(Path::new("example.0")));
        assert!(!glob.is_match(Path::new("example.a")));
    }

    #[test]
    fn lsp_glob_negated_character_class() {
        let glob = LspGlob::new("example.[!0-9]").expect("valid glob");
        assert!(glob.is_match(Path::new("example.a")));
        assert!(!glob.is_match(Path::new("example.0")));
    }

    // ── GlobPattern ──────────────────────────────────────────────

    #[test]
    fn glob_pattern_plain_matches_relative_to_root() {
        let pattern =
            GlobPattern::from_value(&serde_json::json!("**/*.rs")).expect("valid pattern");
        let roots = vec![PathBuf::from("/project")];
        assert!(pattern.is_match(Path::new("/project/src/main.rs"), &roots));
    }

    #[test]
    fn glob_pattern_plain_no_match_outside_root() {
        let pattern =
            GlobPattern::from_value(&serde_json::json!("**/*.rs")).expect("valid pattern");
        let roots = vec![PathBuf::from("/project")];
        assert!(!pattern.is_match(Path::new("/other/src/main.rs"), &roots));
    }

    #[test]
    fn glob_pattern_plain_multiple_roots() {
        let pattern =
            GlobPattern::from_value(&serde_json::json!("**/*.rs")).expect("valid pattern");
        let roots = vec![PathBuf::from("/project-a"), PathBuf::from("/project-b")];
        assert!(pattern.is_match(Path::new("/project-a/src/main.rs"), &roots));
        assert!(pattern.is_match(Path::new("/project-b/lib.rs"), &roots));
        assert!(!pattern.is_match(Path::new("/project-c/lib.rs"), &roots));
    }

    #[test]
    fn glob_pattern_relative_matches_under_base() {
        let pattern = GlobPattern::from_value(&serde_json::json!({
            "baseUri": "file:///project",
            "pattern": "**/*.rs"
        }))
        .expect("valid pattern");
        assert!(pattern.is_match(Path::new("/project/src/main.rs"), &[]));
    }

    #[test]
    fn glob_pattern_relative_no_match_outside_base() {
        let pattern = GlobPattern::from_value(&serde_json::json!({
            "baseUri": "file:///project",
            "pattern": "**/*.rs"
        }))
        .expect("valid pattern");
        assert!(!pattern.is_match(Path::new("/other/src/main.rs"), &[]));
    }

    #[test]
    fn glob_pattern_from_value_string() {
        let pattern =
            GlobPattern::from_value(&serde_json::json!("**/*.rs")).expect("valid pattern");
        assert!(matches!(pattern, GlobPattern::Plain(_)));
    }

    #[test]
    fn glob_pattern_from_value_relative() {
        let pattern = GlobPattern::from_value(&serde_json::json!({
            "baseUri": "file:///project",
            "pattern": "**/*.rs"
        }))
        .expect("valid pattern");
        assert!(matches!(pattern, GlobPattern::Relative { .. }));
    }

    // ── WatchKind ────────────────────────────────────────────────

    #[test]
    fn watch_kind_default_all() {
        let wk = WatchKind::from_value(None);
        assert_eq!(wk, WatchKind(WatchKind::ALL));
    }

    #[test]
    fn watch_kind_create_only() {
        let wk = WatchKind::from_value(Some(WatchKind::CREATE));
        assert!(wk.matches(FileChangeType::Created));
        assert!(!wk.matches(FileChangeType::Changed));
        assert!(!wk.matches(FileChangeType::Deleted));
    }

    #[test]
    fn watch_kind_bitmask() {
        let wk = WatchKind::from_value(Some(WatchKind::CREATE | WatchKind::DELETE));
        assert!(wk.matches(FileChangeType::Created));
        assert!(!wk.matches(FileChangeType::Changed));
        assert!(wk.matches(FileChangeType::Deleted));
    }

    // ── is_glob_pattern ─────────────────────────────────────────

    #[test]
    fn is_glob_pattern_detects_metacharacters() {
        assert!(is_glob_pattern("*.sln"));
        assert!(is_glob_pattern("foo?bar"));
        assert!(is_glob_pattern("[abc]"));
        assert!(!is_glob_pattern("Cargo.toml"));
        assert!(!is_glob_pattern("go.mod"));
        assert!(!is_glob_pattern(".gitignore"));
    }

    // ── FileChangeType ───────────────────────────────────────────

    #[test]
    fn file_change_type_repr() {
        assert_eq!(FileChangeType::Created as u8, 1);
        assert_eq!(FileChangeType::Changed as u8, 2);
        assert_eq!(FileChangeType::Deleted as u8, 3);
    }
}
