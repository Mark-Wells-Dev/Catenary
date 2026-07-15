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

/// The running binary's version — the single source both the daemon snapshot
/// writer ([`crate::state_snapshot::DaemonInfo::current`]) and the skew check
/// read.
///
/// The same `git describe` string `catenary version` and `catenary --version`
/// print (`CATENARY_VERSION`), never the bare `CARGO_PKG_VERSION`. Routing both
/// the recorded daemon version and the comparison through one constant makes the
/// "every non-tag build reads as skewed" false positive impossible (tui-rework
/// 09, item 1).
pub const BINARY_VERSION: &str = env!("CATENARY_VERSION");

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

/// A bridge↔daemon version-mismatch finding (ws41-02), or `None` when the two
/// agree (or nothing was recorded).
///
/// The daemon records an observed mismatch into its `state.json` snapshot the
/// moment a bridge's hello disagrees with the version the daemon links; this
/// reads that record back into a persistent [`Finding`] for `catenary doctor`
/// and the TUI board. `bridge` is the bridge version as recorded (`None` for a
/// pre-handshake bridge that carried no version); `daemon` is the daemon's
/// linked `catenary-mcp` version. The direction-aware wording — which side is
/// older and its cure — comes from [`catenary_mcp::version_mismatch`], the one
/// definition the interrupt, this finding, and the `SessionStart` line share.
///
/// A match (or an unrecorded/agreed pairing, `bridge == Some(daemon)`) produces
/// no finding — silence is the healthy state, and the record self-clears once
/// the versions agree.
#[must_use]
pub fn bridge_mismatch_finding(bridge: Option<&str>, daemon: &str) -> Option<Finding> {
    let mismatch = catenary_mcp::version_mismatch(bridge, daemon)?;
    let fix_it = if mismatch.bridge_is_older() {
        "Run `/mcp` (the host command that restarts the bridge) to pick up the new build."
    } else {
        "Bounce or update the `catenary` binary so the daemon links the newer protocol."
    };
    Some(
        Finding::new(
            FindingCode::BridgeVersionMismatch,
            Severity::Warning,
            mismatch.message(),
        )
        .with_fix_it(fix_it.to_string()),
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
    fn bridge_mismatch_matching_versions_is_silent() {
        assert!(bridge_mismatch_finding(Some("2.0.2"), "2.0.2").is_none());
    }

    #[test]
    fn bridge_mismatch_bridge_older_names_mcp() {
        let f = bridge_mismatch_finding(Some("2.0.1"), "2.0.2").expect("mismatch finding");
        assert_eq!(f.code, FindingCode::BridgeVersionMismatch);
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.message.contains("bridge is older"), "msg: {}", f.message);
        assert!(f.fix_it.as_deref().unwrap_or_default().contains("/mcp"));
    }

    #[test]
    fn bridge_mismatch_daemon_older_names_the_binary() {
        let f = bridge_mismatch_finding(Some("2.1.0"), "2.0.2").expect("mismatch finding");
        assert!(f.message.contains("daemon is older"), "msg: {}", f.message);
        assert!(
            f.fix_it
                .as_deref()
                .unwrap_or_default()
                .contains("catenary` binary")
        );
    }

    #[test]
    fn bridge_mismatch_pre_handshake_bridge_is_a_finding() {
        let f = bridge_mismatch_finding(None, "2.0.2").expect("pre-handshake reads as mismatch");
        assert_eq!(f.code, FindingCode::BridgeVersionMismatch);
        assert!(f.fix_it.as_deref().unwrap_or_default().contains("/mcp"));
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
