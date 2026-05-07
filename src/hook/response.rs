// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Response builder for `systemMessage` content.
//!
//! [`SystemMessageBuilder`] composes two content surfaces into a single
//! string for the host CLI's `systemMessage` field:
//!
//! - **Direct** — lines built synchronously by the current hook handler
//!   (e.g., config validation warnings at `SessionStart`).
//! - **Background** — lines drained from the notification queue,
//!   accumulated since the last drain point.
//!
//! Visual separation: direct lines come first (they describe what the user
//! just triggered), followed by a header and background lines ("oh by the
//! way, these accumulated").

use crate::logging::Severity;

/// Background section header: 3 em-dashes, space, "background", space, 3 em-dashes.
const BACKGROUND_HEADER: &str = "─── background ───";

/// Builder for `systemMessage` content delivered through hook responses.
///
/// Combines direct (synchronous handler messages) and background (notification
/// queue drain) surfaces into a single string. The builder is used on the
/// server side for queue draining and on the CLI side for final composition.
///
/// # Composition rules
///
/// | Direct | Background | Output |
/// |--------|------------|--------|
/// | empty  | empty      | `None` — field omitted |
/// | present| empty      | Direct lines joined by `\n` |
/// | empty  | present    | Header + background lines |
/// | present| present    | Direct lines + separator + header + background lines |
pub struct SystemMessageBuilder {
    direct: Vec<String>,
    background: Vec<String>,
}

impl SystemMessageBuilder {
    /// Create an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            direct: Vec::new(),
            background: Vec::new(),
        }
    }

    /// Append a line built synchronously by this handler.
    ///
    /// Rendered as `[severity] message`.
    pub fn push_direct(&mut self, severity: Severity, message: &str) {
        self.direct.push(format!("[{}] {message}", severity.tag()));
    }

    /// Add a pre-rendered background line.
    ///
    /// Used on the CLI side to reconstitute background content received
    /// from the server's IPC response.
    pub fn push_background(&mut self, line: String) {
        self.background.push(line);
    }

    /// Finalize into the `systemMessage` content string.
    ///
    /// Returns `None` if both surfaces are empty — no `systemMessage`
    /// field should be emitted.
    #[must_use]
    pub fn finish(self) -> Option<String> {
        let has_direct = !self.direct.is_empty();
        let has_background = !self.background.is_empty();

        match (has_direct, has_background) {
            (false, false) => None,
            (true, false) => Some(self.direct.join("\n")),
            (false, true) => {
                let mut out = String::from(BACKGROUND_HEADER);
                out.push('\n');
                out.push_str(&self.background.join("\n"));
                Some(out)
            }
            (true, true) => {
                let mut out = self.direct.join("\n");
                out.push_str("\n\n");
                out.push_str(BACKGROUND_HEADER);
                out.push('\n');
                out.push_str(&self.background.join("\n"));
                Some(out)
            }
        }
    }
}

impl Default for SystemMessageBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Composition unit tests ─────────────────────────────────────────

    #[test]
    fn empty_builder_returns_none() {
        let builder = SystemMessageBuilder::new();
        assert!(builder.finish().is_none());
    }

    #[test]
    fn direct_only_no_header() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Warn, "config: invalid TOML");
        let result = builder.finish();
        assert_eq!(result.as_deref(), Some("[warn] config: invalid TOML"));
    }

    #[test]
    fn multiple_direct_lines_joined() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Error, "config: removed `inherit` field");
        builder.push_direct(Severity::Warn, "config: deprecated key");
        let result = builder.finish().expect("should have content");
        assert!(result.starts_with("[err]"));
        assert!(result.contains('\n'));
        assert!(result.contains("[warn]"));
        assert!(!result.contains("background"));
    }

    #[test]
    fn background_only_has_header() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_background("[warn] rust-analyzer offline".into());
        let result = builder.finish().expect("should have content");
        assert!(result.starts_with("─── background ───\n"));
        assert!(result.contains("[warn] rust-analyzer offline"));
    }

    #[test]
    fn direct_and_background_separated() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Error, "config error");
        builder.push_background("[warn] server crashed".into());
        let result = builder.finish().expect("should have content");
        let parts: Vec<&str> = result.split("\n\n").collect();
        assert_eq!(
            parts.len(),
            2,
            "expected separator between direct and background"
        );
        assert!(parts[0].starts_with("[err]"));
        assert!(parts[1].starts_with("─── background ───"));
        assert!(parts[1].contains("[warn] server crashed"));
    }
}
