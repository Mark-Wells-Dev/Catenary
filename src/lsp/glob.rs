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
    /// The source pattern string, retained verbatim.
    ///
    /// A compiled matcher answers "does this path match?" but not "which
    /// literal paths *could* match?" — the question the supplemental
    /// watch-observation probe planner asks (bug 143;
    /// [`crate::lsp::watch_probe`]).
    source: String,
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
        Ok(Self {
            matcher,
            source: pattern.to_string(),
        })
    }

    /// Tests whether a path matches this pattern.
    #[must_use]
    pub fn is_match(&self, path: &Path) -> bool {
        self.matcher.is_match(path)
    }

    /// Returns the source pattern string this glob was compiled from.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
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

    /// Compiles `pattern` as a [`Plain`](Self::Plain) workspace-relative glob.
    ///
    /// Used to gracefully degrade an object-form `globPattern` whose `baseUri`
    /// is missing or non-`file://`: the `baseUri` is dropped and the `pattern`
    /// is matched workspace-relative (the pre-relative-pattern behavior). A
    /// pattern that itself won't compile still surfaces the error so the caller
    /// can drop the watcher rather than build a broken matcher.
    ///
    /// # Errors
    ///
    /// Returns an error if `pattern` is not a valid glob.
    pub fn plain(pattern: &str) -> Result<Self> {
        Ok(Self::Plain(LspGlob::new(pattern)?))
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

    /// Tests whether a change at `rel`/`abs` matches this pattern, where `rel`
    /// is the path relative to its workspace root and `abs` is the absolute
    /// path.
    ///
    /// For `Plain`, the root-relative `rel` is matched directly; the absolute
    /// `abs` is also tried as a fallback because servers register patterns in
    /// both forms. For `Relative`, `base` is stripped from `abs` and the
    /// remainder is matched. Unlike [`is_match`](Self::is_match) this needs no
    /// workspace-root list — the caller already supplies the root-relative path.
    #[must_use]
    pub fn matches_paths(&self, rel: &Path, abs: &Path) -> bool {
        match self {
            Self::Plain(glob) => glob.is_match(rel) || glob.is_match(abs),
            Self::Relative { base, pattern } => abs
                .strip_prefix(base)
                .is_ok_and(|sub| pattern.is_match(sub)),
        }
    }
}

/// Converts a `file://` URI to a filesystem path.
///
/// The URI is percent-decoded (e.g. `%20` → a space) so a `baseUri` like
/// `file:///home/u/my%20project` resolves to the real `/home/u/my project`.
///
/// Crate-visible since bug 146: the open-document sweep stats the paths behind
/// the URIs a client holds open, and it must decode them exactly as watcher
/// bases are decoded — one converter, no second spelling to drift.
pub(crate) fn uri_to_path(uri: &str) -> Result<PathBuf> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("expected file:// URI, got: {uri}"))?;
    Ok(PathBuf::from(percent_decode(encoded)))
}

/// Percent-decodes a URI path component (`%XX` → byte).
///
/// Decodes valid `%`-escapes to their byte value and reassembles the result as
/// UTF-8; an invalid or truncated escape is left verbatim. Sufficient for the
/// `file://` paths Catenary handles without pulling in a dependency.
///
/// Path separators are decoded too: `%2F` becomes `/`, consistent with standard
/// URL path decoding. A `baseUri` that encodes a slash therefore resolves to a
/// real path separator rather than a literal `%2F` segment.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            // Both nibbles parsed as hex; `hi`/`lo` are each < 16 so the
            // combined value fits in a u8.
            #[allow(clippy::cast_possible_truncation, reason = "hi<16, lo<16 ⇒ byte")]
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    // ── matches_paths (WS31 review C2) ───────────────────────────

    #[test]
    fn ws31_review_c2_matches_paths_plain_no_cross_segment() {
        let pattern = GlobPattern::from_value(&serde_json::json!("*.json")).expect("valid pattern");
        // Top-level root-relative path matches.
        assert!(pattern.matches_paths(Path::new("b.json"), Path::new("/root/b.json")));
        // Nested path must not match — `*` does not cross `/`.
        assert!(!pattern.matches_paths(Path::new("a/b.json"), Path::new("/root/a/b.json")));
    }

    #[test]
    fn ws31_review_c2_matches_paths_relative_strips_base() {
        let pattern = GlobPattern::from_value(&serde_json::json!({
            "baseUri": "file:///project",
            "pattern": "**/*.rs"
        }))
        .expect("valid pattern");
        assert!(pattern.matches_paths(Path::new("src/main.rs"), Path::new("/project/src/main.rs")));
        assert!(!pattern.matches_paths(Path::new("src/main.rs"), Path::new("/other/src/main.rs")));
    }

    // ── uri_to_path percent-decoding (WS31 review C2 / lsp-3) ─────

    #[test]
    fn ws31_review_c2_uri_to_path_percent_decodes() {
        let path = uri_to_path("file:///home/u/my%20project").expect("valid uri");
        assert_eq!(path, PathBuf::from("/home/u/my project"));
    }

    #[test]
    fn ws31_review_c2_uri_to_path_truncated_escape_left_verbatim() {
        // A `%` with no following hex pair is left as-is rather than dropped.
        let path = uri_to_path("file:///a/100%").expect("valid uri");
        assert_eq!(path, PathBuf::from("/a/100%"));
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
}
