// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Service-surface health: unit currency and supervision drift (ws49-04a).
//!
//! The always-on service meets the upgrade lifecycle in two places, and both
//! resolve here into typed findings over an observation the caller gathers
//! ([`ServiceObservation`]) — pure, so every state is unit-testable on any host:
//!
//! - **Currency** (the 2026-07-30 staleness analysis). An installed unit is a
//!   *snapshot*: it goes stale when the `service.rs` generators change (template
//!   drift — the on-disk file predates the build) and when the `ExecStart`
//!   target it baked at install time no longer names the binary the daemon
//!   actually runs (path drift — a binary migration strands it).
//!   Packaging-carried units are immune (brew regenerates its plist per formula
//!   update; a pacman-shipped unit updates with the package), so only the manual
//!   `catenary service install` path drifts — and the cure is that same
//!   idempotent re-run. One calm finding names whichever drift(s) hold.
//! - **Supervision drift.** A daemon can be running while the installed unit
//!   sits inactive: up, but outside the manager, with nothing to relaunch it
//!   when it dies (the live 2026-08-04 specimen). The supervision-aware bounce
//!   closes that window at the *next* `catenary restart` — doctor names it in
//!   between.
//!
//! Silence is the healthy state: nothing installed, or a current unit under a
//! manager that reports it active, produces no findings. So does an unknowable
//! manager state (no user bus): the model never fabricates "inactive".

use crate::health::{Finding, FindingCode, Severity, StaleDiff};

/// What the caller observed about the installed service and the running daemon.
///
/// Borrowed throughout — the caller owns the strings it read from disk, the
/// snapshot, and the generators. Every field is optional in the "could not know"
/// sense, and each check simply does not fire when its inputs are absent.
#[derive(Debug, Default, Clone, Copy)]
pub struct ServiceObservation<'a> {
    /// The manager's label, for prose (`systemd --user`, `launchd (LaunchAgent)`).
    pub manager: &'a str,
    /// The manager's word for its on-disk artifact (`unit` / `plist`).
    pub artifact: &'a str,
    /// The installed file's path, for the fix-it line. `None` when nothing is
    /// installed.
    pub installed_path: Option<&'a str>,
    /// The installed file's content. `None` when nothing is installed (or it
    /// could not be read).
    pub installed: Option<&'a str>,
    /// What this build's generators produce **for the installed file's own
    /// target** — so the diff isolates template drift from path drift.
    pub expected: Option<&'a str>,
    /// The daemon binary the installed file targets.
    pub installed_target: Option<&'a str>,
    /// The binary the running daemon was exec'd from.
    pub running_binary: Option<&'a str>,
    /// Whether the manager reports the service running. `None` when the manager
    /// could not be asked — never guessed.
    pub manager_active: Option<bool>,
    /// Whether a daemon is running at all (a pid on the snapshot).
    pub daemon_running: bool,
}

/// Every service finding the observation supports (0–2), most-actionable first.
///
/// Nothing installed ⇒ empty: an absent service is a supported configuration
/// (spawn-on-demand), not a fault.
#[must_use]
pub fn service_findings(obs: &ServiceObservation<'_>) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(currency_finding(obs));
    out.extend(unsupervised_finding(obs));
    out
}

/// The one calm currency finding: template drift, path drift, or both.
///
/// `None` when nothing is installed, when the inputs to both comparisons are
/// absent, or when the installed file is current — the healthy state is silence.
#[must_use]
pub fn currency_finding(obs: &ServiceObservation<'_>) -> Option<Finding> {
    let installed = obs.installed?;
    let mut reasons: Vec<String> = Vec::new();

    // Template drift: the file on disk versus what this build would write for
    // the very same target.
    let template_drift = obs.expected.is_some_and(|expected| expected != installed);
    if template_drift {
        reasons.push(format!(
            "its template predates this build's {} generator",
            obs.artifact,
        ));
    }

    // Path drift: the baked target versus the binary the daemon actually runs.
    // Both sides must be known; the comparison is skipped, not guessed, when the
    // daemon is down or the platform cannot read its exe.
    if let (Some(target), Some(running)) = (obs.installed_target, obs.running_binary)
        && target != running
    {
        reasons.push(format!(
            "it runs `{target}` but the daemon runs `{running}`"
        ));
    }

    if reasons.is_empty() {
        return None;
    }

    let manager = obs.manager;
    let artifact = obs.artifact;
    let where_at = obs
        .installed_path
        .map_or_else(String::new, |p| format!(" ({p})"));
    let mut finding = Finding::new(
        FindingCode::ServiceUnitStale,
        Severity::Warning,
        format!(
            "the installed {manager} {artifact} is stale{where_at} — {}",
            reasons.join("; "),
        ),
    )
    .with_fix_it(
        "Run `catenary service install` — it is idempotent and rewrites the \
         file from this build."
            .to_string(),
    );
    if template_drift && let Some(expected) = obs.expected {
        finding = finding.with_diff(StaleDiff {
            installed: installed.to_string(),
            expected: expected.to_string(),
        });
    }
    Some(finding)
}

/// The supervision-drift sibling: a daemon running outside an installed but
/// inactive service.
///
/// `None` unless all three hold — installed, a daemon running, and the manager
/// reporting it NOT active. An unknowable manager state stays silent.
#[must_use]
pub fn unsupervised_finding(obs: &ServiceObservation<'_>) -> Option<Finding> {
    if obs.installed.is_none() || !obs.daemon_running || obs.manager_active? {
        return None;
    }
    let manager = obs.manager;
    let artifact = obs.artifact;
    Some(
        Finding::new(
            FindingCode::ServiceUnsupervised,
            Severity::Warning,
            format!(
                "a daemon is running unsupervised — the installed {manager} \
                 {artifact} is inactive, so nothing relaunches the daemon if it dies"
            ),
        )
        .with_fix_it(
            "Run `catenary restart` — the bounce delegates to the service \
             manager, adopting the daemon into it."
                .to_string(),
        ),
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// An observation for an installed, current, supervised service.
    fn healthy<'a>(unit: &'a str, exe: &'a str) -> ServiceObservation<'a> {
        ServiceObservation {
            manager: "systemd --user",
            artifact: "unit",
            installed_path: Some("/home/u/.config/systemd/user/catenary.service"),
            installed: Some(unit),
            expected: Some(unit),
            installed_target: Some(exe),
            running_binary: Some(exe),
            manager_active: Some(true),
            daemon_running: true,
        }
    }

    #[test]
    fn nothing_installed_is_silent() {
        let obs = ServiceObservation {
            manager: "systemd --user",
            artifact: "unit",
            daemon_running: true,
            manager_active: Some(false),
            ..ServiceObservation::default()
        };
        assert!(
            service_findings(&obs).is_empty(),
            "an absent service is a supported configuration, not a fault",
        );
    }

    #[test]
    fn current_and_supervised_is_silent() {
        let obs = healthy("UNIT", "/usr/bin/catenary");
        assert!(
            service_findings(&obs).is_empty(),
            "the healthy state produces no findings",
        );
    }

    #[test]
    fn template_drift_is_one_calm_finding_with_a_diff() {
        let mut obs = healthy("OLD UNIT", "/usr/bin/catenary");
        obs.expected = Some("NEW UNIT");
        let findings = service_findings(&obs);
        assert_eq!(findings.len(), 1, "one calm finding: {findings:?}");
        let f = &findings[0];
        assert_eq!(f.code, FindingCode::ServiceUnitStale);
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.message.contains("template"), "msg: {}", f.message);
        assert!(
            f.fix_it
                .as_deref()
                .unwrap_or_default()
                .contains("catenary service install"),
            "the cure is the idempotent re-run: {:?}",
            f.fix_it,
        );
        let diff = f.diff.as_ref().expect("template drift carries a diff");
        assert_eq!(diff.installed, "OLD UNIT");
        assert_eq!(diff.expected, "NEW UNIT");
    }

    #[test]
    fn exec_start_path_drift_names_both_binaries() {
        let mut obs = healthy("UNIT", "/old/bin/catenary");
        obs.running_binary = Some("/new/bin/catenary");
        let f = currency_finding(&obs).expect("path drift is a finding");
        assert_eq!(f.code, FindingCode::ServiceUnitStale);
        assert!(f.message.contains("/old/bin/catenary"), "{}", f.message);
        assert!(f.message.contains("/new/bin/catenary"), "{}", f.message);
        assert!(
            f.diff.is_none(),
            "path drift alone carries no template diff",
        );
    }

    #[test]
    fn both_drifts_collapse_into_one_finding() {
        let mut obs = healthy("OLD UNIT", "/old/bin/catenary");
        obs.expected = Some("NEW UNIT");
        obs.running_binary = Some("/new/bin/catenary");
        let findings = service_findings(&obs);
        assert_eq!(findings.len(), 1, "still one calm finding: {findings:?}");
        assert!(findings[0].message.contains("template"));
        assert!(findings[0].message.contains("/new/bin/catenary"));
    }

    #[test]
    fn an_unreadable_running_binary_skips_the_target_comparison() {
        let mut obs = healthy("UNIT", "/usr/bin/catenary");
        obs.running_binary = None;
        assert!(
            currency_finding(&obs).is_none(),
            "an unknown running binary is not drift — the comparison is skipped",
        );
    }

    #[test]
    fn daemon_outside_an_inactive_unit_is_named() {
        let mut obs = healthy("UNIT", "/usr/bin/catenary");
        obs.manager_active = Some(false);
        let findings = service_findings(&obs);
        assert_eq!(findings.len(), 1, "only the drift finding: {findings:?}");
        let f = &findings[0];
        assert_eq!(f.code, FindingCode::ServiceUnsupervised);
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.message.contains("unsupervised"), "msg: {}", f.message);
        assert!(
            f.fix_it
                .as_deref()
                .unwrap_or_default()
                .contains("catenary restart"),
            "the cure is the adopting bounce: {:?}",
            f.fix_it,
        );
    }

    #[test]
    fn an_inactive_unit_with_no_daemon_is_silent() {
        let mut obs = healthy("UNIT", "/usr/bin/catenary");
        obs.manager_active = Some(false);
        obs.daemon_running = false;
        obs.running_binary = None;
        assert!(
            unsupervised_finding(&obs).is_none(),
            "a stopped daemon under an inactive unit is just stopped",
        );
    }

    #[test]
    fn an_unknowable_manager_state_never_fabricates_drift() {
        let mut obs = healthy("UNIT", "/usr/bin/catenary");
        obs.manager_active = None;
        assert!(
            unsupervised_finding(&obs).is_none(),
            "no user bus ⇒ unknown, never 'inactive'",
        );
    }
}
