// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostic message compression — a generic line-strip engine driven by
//! manifest data (diagnostics-debt 04 / misc 165's rider).
//!
//! LSP servers attach boilerplate to diagnostic messages (reference URLs, lint
//! attribution) that wastes tokens when delivered to AI agents. This module is a
//! **message compressor** with fixed, narrow semantics:
//!
//! - a message **line** whose trimmed text matches a manifest
//!   [`StripRule`](crate::recipes::StripRule) is deleted whole;
//! - a message left **empty** after stripping drops entirely (the
//!   standalone-attribution case);
//! - nothing is ever **rewritten or injected**;
//! - a server/version outside its manifest pin is **pass-through**.
//!
//! The per-server rules are not code here — they ride the blessed manifest
//! (`defaults/blessed-manifest.toml`, `[[discipline.<server>.compress]]`), the
//! declared-constants species: regexes against an output format Catenary does not
//! own, verified per re-pin. This engine stays in the binary; the rules travel
//! with the pin, so a re-pin ships updated compression without a binary release,
//! and the embedded seed carries the rules offline. The doctrine line: **the
//! filter's mandate is compression, never verdict policy** — a stale manifest can
//! only let boilerplate reflood, never eat content.
//!
//! The severity threshold ([`severity_passes`]) stays user config — a separate
//! lever this engine never touches.

/// LSP severity constants (1=Error through 4=Hint).
pub const SEVERITY_ERROR: u8 = 1;
/// Warning severity.
pub const SEVERITY_WARNING: u8 = 2;
/// Information severity.
pub const SEVERITY_INFORMATION: u8 = 3;
/// Hint severity.
pub const SEVERITY_HINT: u8 = 4;

/// LSP diagnostic code, which can be a number or a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticCode {
    /// Numeric diagnostic code (e.g., TypeScript `6133`).
    Number(i64),
    /// String diagnostic code (e.g., clippy `"needless_return"`).
    Text(String),
}

impl DiagnosticCode {
    /// Converts from a JSON diagnostic code value.
    #[must_use]
    pub fn from_value(code: &serde_json::Value) -> Self {
        code.as_i64().map_or_else(
            || {
                code.as_str().map_or_else(
                    || Self::Text(code.to_string()),
                    |s| Self::Text(s.to_string()),
                )
            },
            Self::Number,
        )
    }
}

/// Compresses noise from a diagnostic message using the server's manifest
/// [`StripRule`](crate::recipes::StripRule)s.
///
/// Deletes whole message lines that match any of the server's strip rules; a
/// message that is empty after stripping returns `""` (a signal to the caller to
/// drop the diagnostic entirely). Passes the message through **unchanged** when
/// the server has no rules, or when `version` is outside the rules' pinned range
/// ([`DisciplineRecord::compress_applies`](crate::recipes::DisciplineRecord::compress_applies))
/// — the version safety the hand-coded rules carried, now manifest data. The
/// engine never rewrites text or fires on `source`/`code`/`severity`; those
/// remain available to a caller but are not consulted, keeping the compressor's
/// mandate narrow (compression, never verdict policy).
///
/// # Return value
///
/// - Non-empty string: deliver this message (original or line-stripped).
/// - Empty string: drop the diagnostic entirely.
#[must_use]
pub fn compress_message(server: &str, version: Option<&str>, message: &str) -> String {
    let record = crate::recipes::seed_manifest().discipline_for(server);
    if record.compress.is_empty() || !record.compress_applies(version) {
        return message.to_string();
    }
    message
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !record.compress.iter().any(|rule| rule.matches(trimmed))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// Trait for compressing noise from LSP diagnostic messages.
///
/// A thin seam over [`compress_message`] so call sites can hold a `&dyn`
/// compressor. There is a single production implementation
/// ([`ManifestFilter`]) — the per-server behavior lives in the manifest, not in
/// distinct trait impls.
///
/// # Return value
///
/// - Non-empty string: deliver this message (original or rewritten).
/// - Empty string: drop the diagnostic entirely.
#[allow(
    clippy::too_many_arguments,
    reason = "diagnostic context requires all fields"
)]
pub trait DiagnosticFilter: Send + Sync {
    /// Compresses noise from a diagnostic message.
    ///
    /// The manifest engine consults only `server`, `version`, and `message`; the
    /// remaining fields (`source`, `code`, `severity`, `language_id`) are part of
    /// the diagnostic context a caller carries and are accepted for signature
    /// stability, but the compressor deliberately does not rule on them.
    fn filter_message(
        &self,
        server: &str,
        version: Option<&str>,
        source: Option<&str>,
        code: Option<&DiagnosticCode>,
        severity: u8,
        language_id: &str,
        message: &str,
    ) -> String;
}

/// The manifest-driven compressor — the single [`DiagnosticFilter`]
/// implementation.
///
/// Delegates to [`compress_message`], which reads the server's strip rules from
/// the manifest. Zero-sized: it holds no per-server state, so one static instance
/// serves every server.
pub struct ManifestFilter;

impl DiagnosticFilter for ManifestFilter {
    fn filter_message(
        &self,
        server: &str,
        version: Option<&str>,
        _source: Option<&str>,
        _code: Option<&DiagnosticCode>,
        _severity: u8,
        _language_id: &str,
        message: &str,
    ) -> String {
        compress_message(server, version, message)
    }
}

/// Returns the diagnostic compressor for a server command.
///
/// A single manifest-driven implementation serves every server — the per-server
/// behavior is the server's manifest strip rules, not a distinct impl. Kept as a
/// lookup so call sites hold a `&dyn DiagnosticFilter` uniformly.
#[must_use]
pub fn get_filter(_server_command: &str) -> &'static dyn DiagnosticFilter {
    static MANIFEST: ManifestFilter = ManifestFilter;
    &MANIFEST
}

/// Parses a severity string from config into a `u8` (LSP severity encoding).
///
/// Returns `None` for unrecognized values (caller should treat as "no threshold").
#[must_use]
pub fn parse_severity(s: &str) -> Option<u8> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Some(SEVERITY_ERROR),
        "warning" => Some(SEVERITY_WARNING),
        "information" | "info" => Some(SEVERITY_INFORMATION),
        "hint" => Some(SEVERITY_HINT),
        _ => None,
    }
}

/// Returns `true` if the diagnostic severity meets or exceeds the threshold.
///
/// LSP severity is inverted: 1 = Error (most severe), 4 = Hint (least).
/// A diagnostic passes if its severity value is ≤ the threshold value.
#[must_use]
pub const fn severity_passes(severity: u8, threshold: u8) -> bool {
    severity_rank(severity) <= severity_rank(threshold)
}

/// Maps severity to a numeric rank for comparison.
/// Lower rank = more severe (Error=1, Warning=2, Info=3, Hint=4, unknown=5).
const fn severity_rank(s: u8) -> u8 {
    match s {
        1..=4 => s,
        _ => 5,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// Compress a message for `server` at `version` through the manifest engine.
    fn compress(server: &str, version: Option<&str>, message: &str) -> String {
        compress_message(server, version, message)
    }

    // ── the generic engine over the pinned rust-analyzer rules ──────────

    #[test]
    fn strips_clippy_url() {
        let msg = "unused variable `x`\nfor further information visit https://rust-lang.github.io/rust-clippy/master/index.html#unused_variable";
        assert_eq!(
            compress("rust-analyzer", Some("1.92.0"), msg),
            "unused variable `x`"
        );
    }

    #[test]
    fn strips_lint_attribution_on_by_default() {
        let msg = "unused variable `x`\n`#[warn(unused_variables)]` on by default";
        assert_eq!(
            compress("rust-analyzer", Some("1.92.0"), msg),
            "unused variable `x`"
        );
    }

    #[test]
    fn strips_lint_attribution_implied_by() {
        let msg =
            "unused variable `x`\n`#[warn(clippy::pedantic)]` implied by `#[warn(clippy::all)]`";
        assert_eq!(
            compress("rust-analyzer", Some("1.92.0"), msg),
            "unused variable `x`"
        );
    }

    #[test]
    fn strips_lint_attribution_to_override() {
        let msg = "unused variable `x`\n`#[allow(unused)]` to override `#[warn(unused_variables)]`";
        assert_eq!(
            compress("rust-analyzer", Some("1.92.0"), msg),
            "unused variable `x`"
        );
    }

    #[test]
    fn strips_multiple_noise_lines() {
        let msg = "needless return\nfor further information visit https://example.com\n`#[warn(clippy::needless_return)]` on by default";
        assert_eq!(
            compress("rust-analyzer", Some("1.92.0"), msg),
            "needless return"
        );
    }

    #[test]
    fn drops_standalone_w_flag_implied_by() {
        let msg = "`-W clippy::doc-markdown` implied by `-W clippy::pedantic`";
        assert_eq!(compress("rust-analyzer", Some("1.92.0"), msg), "");
    }

    #[test]
    fn drops_standalone_to_override_with_allow() {
        let msg = "to override `-W clippy::pedantic` add `#[allow(clippy::doc_markdown)]`";
        assert_eq!(compress("rust-analyzer", Some("1.92.0"), msg), "");
    }

    #[test]
    fn drops_standalone_to_override_with_warn() {
        let msg = "to override `-D warnings` add `#[warn(clippy::doc_markdown)]`";
        assert_eq!(compress("rust-analyzer", Some("1.92.0"), msg), "");
    }

    // ── preserve: a clean message survives byte-exact ───────────────────

    #[test]
    fn preserves_clean_message() {
        let msg = "expected `usize`, found `&str`";
        assert_eq!(compress("rust-analyzer", Some("1.92.0"), msg), msg);
    }

    #[test]
    fn preserves_multiline_clean_message() {
        let msg = "mismatched types\nexpected `usize`\n   found `&str`";
        assert_eq!(compress("rust-analyzer", Some("1.92.0"), msg), msg);
    }

    // ── version safety: out-of-pin and unknown versions pass through ────

    #[test]
    fn passthrough_for_unknown_version() {
        let msg = "unused variable `x`\nfor further information visit https://example.com";
        assert_eq!(compress("rust-analyzer", Some("2.0.0"), msg), msg);
    }

    #[test]
    fn passthrough_for_no_version() {
        let msg = "unused variable `x`\nfor further information visit https://example.com";
        assert_eq!(compress("rust-analyzer", None, msg), msg);
    }

    // ── directional safety: an unverified/unruled server never strips ───

    #[test]
    fn passthrough_for_unruled_server() {
        // A server with no compression rules in the manifest passes every
        // message through unchanged — the compressor never invents rules.
        let msg = "unused variable `x`\nfor further information visit https://example.com";
        assert_eq!(compress("gopls", Some("v0.22.0"), msg), msg);
        assert_eq!(compress("some-custom-server", Some("1.0.0"), msg), msg);
    }

    // ── the trait seam and the get_filter lookup ────────────────────────

    #[test]
    fn manifest_filter_matches_direct_compress() {
        let filter = get_filter("rust-analyzer");
        let msg = "unused variable `x`\nfor further information visit https://example.com";
        let result = filter.filter_message(
            "rust-analyzer",
            Some("1.92.0"),
            Some("clippy"),
            None,
            SEVERITY_WARNING,
            "rust",
            msg,
        );
        assert_eq!(result, "unused variable `x`");
    }

    #[test]
    fn get_filter_passes_unknown_server_through() {
        let filter = get_filter("unknown-server");
        let result = filter.filter_message(
            "unknown-server",
            None,
            None,
            None,
            SEVERITY_ERROR,
            "python",
            "syntax error",
        );
        assert_eq!(result, "syntax error");
    }

    // ── severity + code helpers (unchanged, user-config lever) ──────────

    #[test]
    fn parse_severity_valid() {
        assert_eq!(parse_severity("error"), Some(SEVERITY_ERROR));
        assert_eq!(parse_severity("Warning"), Some(SEVERITY_WARNING));
        assert_eq!(parse_severity("information"), Some(SEVERITY_INFORMATION));
        assert_eq!(parse_severity("info"), Some(SEVERITY_INFORMATION));
        assert_eq!(parse_severity("hint"), Some(SEVERITY_HINT));
    }

    #[test]
    fn parse_severity_invalid() {
        assert_eq!(parse_severity("bogus"), None);
    }

    #[test]
    fn severity_passes_threshold() {
        assert!(severity_passes(SEVERITY_ERROR, SEVERITY_WARNING));
        assert!(severity_passes(SEVERITY_WARNING, SEVERITY_WARNING));
        assert!(!severity_passes(SEVERITY_HINT, SEVERITY_WARNING));
        assert!(!severity_passes(SEVERITY_INFORMATION, SEVERITY_WARNING));
    }

    #[test]
    fn diagnostic_code_from_value() {
        use serde_json::json;

        assert_eq!(
            DiagnosticCode::from_value(&json!(6133)),
            DiagnosticCode::Number(6133)
        );
        assert_eq!(
            DiagnosticCode::from_value(&json!("needless_return")),
            DiagnosticCode::Text("needless_return".to_string())
        );
    }
}
