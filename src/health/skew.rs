// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Version-skew computation: this binary versus the running daemon.
//!
//! The TUI binary *is* the CLI binary, so this binary's version is known for
//! free; the daemon's version arrives through the [`HealthFeed`] seam
//! ([`crate::health::servers::HealthFeed::daemon_version`]). A known mismatch is
//! a warning finding. When the daemon version is unknown — no daemon running, or
//! a daemon predating `tool/version` — there is nothing to compare, so no
//! finding is produced.

use crate::health::{Finding, FindingCode, Severity};

/// A version-skew finding when `daemon_version` is present and differs from
/// `binary_version`; otherwise `None`.
///
/// A match, or an absent daemon version, produces no finding — silence is the
/// healthy (and the unknowable) state.
#[must_use]
pub fn skew_finding(binary_version: &str, daemon_version: Option<&str>) -> Option<Finding> {
    let daemon = daemon_version?;
    if daemon == binary_version {
        return None;
    }
    Some(
        Finding::new(
            FindingCode::VersionSkew,
            Severity::Warning,
            format!("daemon is stale — running {daemon}, this binary is {binary_version}"),
        )
        .with_fix_it("Run `catenary stop` (or restart it) to pick up the new build.".to_string()),
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_produce_no_finding() {
        assert!(skew_finding("1.3.6", Some("1.3.6")).is_none());
    }

    #[test]
    fn unknown_daemon_version_produces_no_finding() {
        assert!(skew_finding("1.3.6", None).is_none());
    }

    #[test]
    fn mismatch_produces_warning() {
        let finding = skew_finding("1.3.7", Some("1.3.6-3-gabc1234"));
        let finding = finding.expect("a mismatch produces a finding");
        assert_eq!(finding.code, FindingCode::VersionSkew);
        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.message.contains("stale"));
        assert!(finding.fix_it.is_some());
    }
}
