// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Section-scoped config quarantine (bug 110).
//!
//! A config section that fails *semantic* validation is **quarantined** rather
//! than aborting the whole load: it is replaced by its default/absent form and
//! recorded here with the errors that condemned it. The valid remainder of the
//! document loads normally, so the surfaces that never consume the broken section
//! keep working (`catenary grep`/`glob`), the enforcement surface degrades
//! loudly (the `PreToolUse` hook), and the daemon boots on what parsed.
//!
//! Quarantine is for *semantically-invalid sections of a syntactically-valid
//! document*. A TOML-document-level parse failure (a torn file, invalid syntax)
//! is NOT quarantined — it remains a full refusal, because a document that does
//! not parse has no valid remainder to salvage (bug 111 keeps that path: refuse,
//! fire one notification, clean up sockets).
//!
//! The load-bearing case is `[commands]` (with all its subtables —
//! `deny`/`deny_flags`/`allow_flags`/`guidance`/`script_hosts`): the incident
//! config carried two cross-reference errors there, and its blast radius took
//! down every surface. A consumer that needs to recover a single field
//! best-effort from an otherwise-invalid section re-reads the raw document
//! itself — the hook reads `client_enforcement_only` that way to honour the
//! fail-closed opt-in even when the rest of `[commands]` is condemned.

use serde::Serialize;

/// One config section that failed semantic validation and was defaulted out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuarantinedSection {
    /// The TOML section header, without brackets (e.g. `commands`).
    pub section: String,
    /// The validation error(s) that condemned the section, in order.
    ///
    /// Never empty — a section is only recorded when at least one error fires.
    pub errors: Vec<String>,
}

impl QuarantinedSection {
    /// The first (representative) error, for a one-line surface (notification,
    /// stderr warning, `additionalContext`).
    #[must_use]
    pub fn first_error(&self) -> &str {
        self.errors
            .first()
            .map_or("(no error recorded)", String::as_str)
    }
}

/// The set of sections quarantined during a single config load.
///
/// Attached to the loaded [`Config`](super::Config) so every surface reads the
/// same record. Empty on a clean load — [`is_empty`](Self::is_empty) is the
/// common "did anything degrade?" check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Quarantine {
    sections: Vec<QuarantinedSection>,
}

impl Quarantine {
    /// An empty quarantine (a clean load).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a quarantined section with its non-empty error list.
    ///
    /// A no-op when `errors` is empty — a section with no errors was not
    /// condemned and must not be recorded (that would falsely degrade its
    /// consumers).
    pub fn record(&mut self, section: impl Into<String>, errors: Vec<String>) {
        if errors.is_empty() {
            return;
        }
        self.sections.push(QuarantinedSection {
            section: section.into(),
            errors,
        });
    }

    /// Whether nothing was quarantined (the clean-load fast path).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// The quarantined sections, in the order they were recorded.
    #[must_use]
    pub fn sections(&self) -> &[QuarantinedSection] {
        &self.sections
    }

    /// The record for a named section, or `None` when that section is not
    /// quarantined. Used by consumers that only care about one section (the hook
    /// checks `commands`).
    #[must_use]
    pub fn section(&self, name: &str) -> Option<&QuarantinedSection> {
        self.sections.iter().find(|s| s.section == name)
    }

    /// Whether the named section is quarantined.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.section(name).is_some()
    }

    /// A single-line summary naming the quarantined section(s) and the first
    /// error, for a notification or stderr warning:
    /// `[commands] quarantined: <error>`. Returns `None` on a clean load.
    ///
    /// When several sections are quarantined the summary names them all and
    /// quotes the first section's first error, keeping the line short.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        let first = self.sections.first()?;
        let names = self
            .sections
            .iter()
            .map(|s| format!("[{}]", s.section))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("{names} quarantined: {}", first.first_error()))
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
    fn empty_quarantine_is_clean() {
        let q = Quarantine::new();
        assert!(q.is_empty());
        assert!(q.summary().is_none());
        assert!(!q.contains("commands"));
        assert!(q.section("commands").is_none());
    }

    #[test]
    fn record_condemns_a_section() {
        let mut q = Quarantine::new();
        q.record("commands", vec!["boom".to_string()]);
        assert!(!q.is_empty());
        assert!(q.contains("commands"));
        let section = q.section("commands").expect("commands recorded");
        assert_eq!(section.first_error(), "boom");
    }

    #[test]
    fn record_ignores_empty_error_list() {
        let mut q = Quarantine::new();
        q.record("commands", vec![]);
        assert!(
            q.is_empty(),
            "a section with no errors must not be quarantined"
        );
    }

    #[test]
    fn summary_names_the_section_and_first_error() {
        let mut q = Quarantine::new();
        q.record(
            "commands",
            vec!["first error".to_string(), "second error".to_string()],
        );
        let summary = q.summary().expect("non-empty quarantine has a summary");
        assert!(summary.contains("[commands]"), "{summary}");
        assert!(summary.contains("first error"), "{summary}");
        assert!(
            !summary.contains("second error"),
            "summary quotes only the first error: {summary}"
        );
    }

    #[test]
    fn summary_names_all_quarantined_sections() {
        let mut q = Quarantine::new();
        q.record("commands", vec!["cmd err".to_string()]);
        q.record("notifications", vec!["notif err".to_string()]);
        let summary = q.summary().expect("summary");
        assert!(summary.contains("[commands]"), "{summary}");
        assert!(summary.contains("[notifications]"), "{summary}");
        // The quoted error is the first section's first error.
        assert!(summary.contains("cmd err"), "{summary}");
    }
}
