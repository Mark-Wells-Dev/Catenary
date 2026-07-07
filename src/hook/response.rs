// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Response builder for `systemMessage` content.
//!
//! [`SystemMessageBuilder`] collects the **direct** `systemMessage` lines a hook
//! handler builds synchronously — today only the `SessionStart` config-validation
//! error ("Catenary configuration error … run `catenary doctor`"), a fresh,
//! error-severity notice the conversation is genuinely the right surface for.
//!
//! The **background** surface it used to carry — the notification-queue drain —
//! retired with the queue (tui-rework 04): warns now persist on the TUI health
//! surface and the dirty-worktree parent notice rides `additionalContext`, not
//! `systemMessage`. So this is now a thin `[severity] message` line formatter.

use crate::logging::Severity;

/// Builder for direct `systemMessage` content delivered through hook responses.
///
/// Each line renders as `[severity] message`; [`finish`](Self::finish) joins them
/// with newlines, or returns `None` when empty so no `systemMessage` field is
/// emitted.
#[derive(Default)]
pub struct SystemMessageBuilder {
    direct: Vec<String>,
}

impl SystemMessageBuilder {
    /// Create an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self { direct: Vec::new() }
    }

    /// Append a line built synchronously by this handler.
    ///
    /// Rendered as `[severity] message`.
    pub fn push_direct(&mut self, severity: Severity, message: &str) {
        self.direct.push(format!("[{}] {message}", severity.tag()));
    }

    /// Finalize into the `systemMessage` content string.
    ///
    /// Returns `None` when no lines were pushed — no `systemMessage` field
    /// should be emitted.
    #[must_use]
    pub fn finish(self) -> Option<String> {
        if self.direct.is_empty() {
            None
        } else {
            Some(self.direct.join("\n"))
        }
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
    fn empty_builder_returns_none() {
        let builder = SystemMessageBuilder::new();
        assert!(builder.finish().is_none());
    }

    #[test]
    fn direct_line_rendered_with_severity_tag() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Error, "config: invalid TOML");
        assert_eq!(
            builder.finish().as_deref(),
            Some("[err] config: invalid TOML")
        );
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
    }
}
