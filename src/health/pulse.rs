// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The pulseless-session finding: silent bridge loss gets a name (pulse-05).
//!
//! A session whose hooks keep dispatching while the daemon holds **zero** live
//! MCP bridge connections is running degraded: no workspace-roots channel
//! (mount durability and install pre-warm assumptions quietly void), no MCP
//! notifications, and no heartbeat in the daemon's connection census — the
//! daemon is one visitor-exit away from dying under it (bug 127). This module
//! is the one definition both renderers share (doctor and the TUI, the
//! established health pattern): the condition is derived from state the
//! snapshot already carries — the session board's hook recency
//! ([`SessionEntry::last_seen`]) and the daemon block's `mcp_connections`
//! census — never from new per-session tracking.
//!
//! **Attribution honesty:** the daemon has no MCP-connection↔session binding
//! (a bridge's MCP handshake carries no host session id), so per-session
//! attribution is only provable when the census reads zero — then *every*
//! hook-active, bridge-capable session is bridgeless by definition. With one
//! or more live bridges a partial outage cannot name its victim, so the model
//! stays silent rather than guess. A `None` census (a daemon predating the
//! field, or none wired) is unknown, and unknown is silence.
//!
//! **Once-per-condition discipline** (the linger-nag precedent): the finding
//! is a standing derivation over live state, like the bridge-mismatch record —
//! it surfaces once and persists while the condition holds, clears the moment
//! a bridge connects (the census leaves zero), and re-arms for a fresh outage.
//! No push channel fires: warn-class per the tracing conventions, so no
//! desktop interrupt.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::health::{Finding, FindingCode, Severity};
use crate::state_snapshot::SessionEntry;

/// How recently a session's last hook dispatch must have landed for the
/// session to count as *active* in the pulseless comparison.
///
/// The named injectable default (pulse-05): callers pass it to
/// [`pulseless_sessions`] / [`pulseless_findings`]; tests inject smaller
/// windows. Ten minutes — long enough that a session mid-thought never flaps
/// out, short enough that a session whose agent exited ages out of the finding
/// on its own (which also self-heals a census frozen by daemon death).
pub const PULSELESS_RECENCY_WINDOW: Duration = Duration::from_mins(10);

/// The sessions provably pulseless: hook-active within `window` of `now`,
/// bridge-capable by client, while the daemon's MCP bridge census reads zero.
///
/// Returns an empty list when the census is `None` (unknown — a daemon
/// predating the field) or nonzero (bridged, or a partial outage the census
/// cannot attribute; see the module docs). A session whose `last_seen` fails
/// to parse contributes nothing — no fabricated recency. A `last_seen` ahead
/// of `now` (clock skew, or a dispatch racing the read) counts as recent.
#[must_use]
pub fn pulseless_sessions(
    sessions: &[SessionEntry],
    mcp_connections: Option<u64>,
    now: DateTime<Utc>,
    window: Duration,
) -> Vec<&SessionEntry> {
    if mcp_connections != Some(0) {
        return Vec::new();
    }
    sessions
        .iter()
        .filter(|session| session.client.establishes_mcp_bridge())
        .filter(|session| {
            DateTime::parse_from_rfc3339(&session.last_seen).is_ok_and(|last_seen| {
                // A negative elapsed (future `last_seen`) is Err — recent.
                (now - last_seen.with_timezone(&Utc))
                    .to_std()
                    .map_or(true, |elapsed| elapsed <= window)
            })
        })
        .collect()
}

/// The warn-tier finding for one pulseless session.
///
/// Names the session id and carries the remedy as fix-it guidance.
/// [`Severity::Warning`] matches its sibling snapshot-side findings (degraded
/// enrichment is not urgent; it must simply stop being invisible) — a TUI
/// board finding and a doctor line, never a desktop interrupt.
#[must_use]
pub fn pulseless_finding(session: &SessionEntry) -> Finding {
    Finding::new(
        FindingCode::SessionPulseless,
        Severity::Warning,
        format!(
            "session {}: hook-active with no MCP bridge connected — \
             workspace-roots sync and MCP notifications are down",
            session.id
        ),
    )
    .with_fix_it("Resume the session (or run `/mcp`) to restore its bridge.".to_string())
}

/// The pulseless finding set, one finding per pulseless session.
///
/// [`pulseless_sessions`] mapped through [`pulseless_finding`] — doctor's
/// shape; the TUI uses the two halves directly so it can tag each finding
/// with its client owner.
#[must_use]
pub fn pulseless_findings(
    sessions: &[SessionEntry],
    mcp_connections: Option<u64>,
    now: DateTime<Utc>,
    window: Duration,
) -> Vec<Finding> {
    pulseless_sessions(sessions, mcp_connections, now, window)
        .into_iter()
        .map(pulseless_finding)
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::state_snapshot::ClientInfo;

    /// A board entry for `client`, last seen `secs_ago` seconds before `now`.
    fn session(id: &str, client: &str, secs_ago: i64, now: DateTime<Utc>) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            client: ClientInfo {
                name: client.to_string(),
                version: None,
            },
            last_seen: (now - chrono::Duration::seconds(secs_ago))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ..SessionEntry::default()
        }
    }

    #[test]
    fn hook_active_session_with_zero_census_surfaces_naming_id_and_remedy() {
        let now = Utc::now();
        let sessions = vec![session("sess-A", "claude", 60, now)];
        let findings = pulseless_findings(&sessions, Some(0), now, PULSELESS_RECENCY_WINDOW);
        assert_eq!(findings.len(), 1, "one finding per pulseless session");
        let f = &findings[0];
        assert_eq!(f.code, FindingCode::SessionPulseless);
        assert_eq!(f.severity, Severity::Warning, "warn-class, no interrupt");
        assert!(f.is_problem(), "counts in the health verdict");
        assert!(f.message.contains("sess-A"), "names the session id");
        assert!(
            f.fix_it
                .as_deref()
                .expect("remedy carried")
                .contains("/mcp"),
            "carries the resume-or-/mcp remedy",
        );
    }

    #[test]
    fn bridged_census_clears_and_zero_again_rearms() {
        // Once-per-condition discipline (pulse-05): the finding is a standing
        // derivation — stable while the condition holds (no accumulation), a
        // bridge connecting clears it, and a fresh outage re-arms it.
        let now = Utc::now();
        let sessions = vec![session("sess-A", "claude", 60, now)];
        let first = pulseless_findings(&sessions, Some(0), now, PULSELESS_RECENCY_WINDOW);
        let again = pulseless_findings(&sessions, Some(0), now, PULSELESS_RECENCY_WINDOW);
        assert_eq!(first.len(), 1);
        assert_eq!(
            again.len(),
            1,
            "the standing finding persists — one surfacing, never a growing pile",
        );
        assert!(
            pulseless_findings(&sessions, Some(1), now, PULSELESS_RECENCY_WINDOW).is_empty(),
            "a bridge connecting clears the finding",
        );
        assert_eq!(
            pulseless_findings(&sessions, Some(0), now, PULSELESS_RECENCY_WINDOW).len(),
            1,
            "a fresh outage re-arms it",
        );
    }

    #[test]
    fn idle_session_is_exempt() {
        let now = Utc::now();
        let stale_secs = i64::try_from(PULSELESS_RECENCY_WINDOW.as_secs()).expect("fits") + 1;
        let sessions = vec![session("sess-A", "claude", stale_secs, now)];
        assert!(
            pulseless_findings(&sessions, Some(0), now, PULSELESS_RECENCY_WINDOW).is_empty(),
            "no recent hook activity — the session is idle, not pulseless",
        );
    }

    #[test]
    fn non_bridging_clients_are_exempt_by_capability() {
        let now = Utc::now();
        let sessions = vec![
            session("agy", "antigravity", 60, now),
            session("mystery", "unknown", 60, now),
        ];
        assert!(
            pulseless_findings(&sessions, Some(0), now, PULSELESS_RECENCY_WINDOW).is_empty(),
            "a client that never establishes MCP has no bridge to lose",
        );
    }

    #[test]
    fn unknown_or_nonzero_census_is_silence() {
        let now = Utc::now();
        let sessions = vec![session("sess-A", "claude", 60, now)];
        assert!(
            pulseless_findings(&sessions, None, now, PULSELESS_RECENCY_WINDOW).is_empty(),
            "a daemon predating the census reads as unknown, never as zero",
        );
        assert!(
            pulseless_findings(&sessions, Some(2), now, PULSELESS_RECENCY_WINDOW).is_empty(),
            "with live bridges a partial outage cannot name its victim — silence",
        );
    }

    #[test]
    fn unparseable_last_seen_contributes_nothing() {
        let now = Utc::now();
        let mut entry = session("sess-A", "claude", 60, now);
        entry.last_seen = String::new();
        assert!(
            pulseless_findings(&[entry], Some(0), now, PULSELESS_RECENCY_WINDOW).is_empty(),
            "no fabricated recency from an empty/unparseable last_seen",
        );
    }

    #[test]
    fn future_last_seen_counts_as_recent() {
        // Clock skew (or a dispatch racing the read) must not hide a live
        // session from the comparison.
        let now = Utc::now();
        let sessions = vec![session("sess-A", "claude", -30, now)];
        assert_eq!(
            pulseless_findings(&sessions, Some(0), now, PULSELESS_RECENCY_WINDOW).len(),
            1,
        );
    }
}
