// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary's bridge crate.
//!
//! This crate owns the bridge↔daemon **wire-protocol definition**: the
//! MCP (Model Context Protocol) JSON-RPC message types that cross the Unix
//! domain socket between the host-spawned bridge process and the Catenary
//! daemon, their serialization, and a [protocol version constant]
//! ([`PROTOCOL_VERSION`]). Both sides — the bridge process and the daemon —
//! link this one crate, so they structurally cannot disagree about what the
//! protocol *is*, only about which crate version they were built from.
//!
//! The daemon (in the `catenary-cli` package) imports the protocol definition
//! from here. Daemon-side *behavior* — the IPC hello handler, the version
//! comparison, and the mismatch surfacing — stays in the daemon; this crate
//! ships definitions, never daemon logic.
//!
//! # Version seam
//!
//! The crate's own semver ([`version`]) is the comparand a future handshake
//! compares across the wire: it bumps only when the bridge's wire or behavior
//! changes. It is readable at both build sites — the bridge compiles its
//! version in, and the daemon knows the version it links — so a handshake can
//! be built on top of it without either side re-declaring the value.

/// The bridge crate's own semantic version, as compiled into whichever
/// binary links it.
///
/// This is the comparand for the bridge↔daemon version handshake: the crate's
/// semver bumps only when the bridge's wire or behavior changes, so version
/// equality across the wire is protocol sameness. Exposed as an accessor so
/// both build sites (the bridge process and the daemon) read the identical
/// compiled-in value.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The wire-protocol version carried by the bridge↔daemon handshake.
///
/// A monotonically increasing integer that identifies the shape of the
/// [`protocol`] message set. It is a coarser, hand-maintained comparand than
/// [`version`] (which tracks the crate's full semver): bump it when the wire
/// *format* changes in a way both sides must agree on. The handshake itself
/// (comparison and surfacing) lives daemon-side and is out of this crate's
/// scope — this constant is the value that handshake reads.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod protocol;

/// The bridge↔daemon version comparison, direction-blind (ws41-02, ruling Q4).
///
/// `bridge` is the bridge's compiled [`version`] as it arrived over the wire
/// ([`None`] for a pre-handshake bridge that sent no hello); `daemon` is the
/// [`version`] the daemon links. Equality is protocol sameness, so a match
/// yields [`None`] — silence is the healthy state. Any inequality — including a
/// pre-handshake `None` bridge — yields [`Some`] with the direction the pairing
/// implies, so the surfacing (interrupt, doctor/board finding, `SessionStart`
/// line) can name the older side and its cure. Detection itself does not care
/// which side is older; direction lives only in the returned value.
#[must_use]
pub fn version_mismatch(bridge: Option<&str>, daemon: &str) -> Option<VersionMismatch> {
    match bridge {
        Some(b) if b == daemon => None,
        Some(b) => Some(VersionMismatch {
            bridge: BridgeVersion::Reported(b.to_string()),
            daemon: daemon.to_string(),
        }),
        None => Some(VersionMismatch {
            bridge: BridgeVersion::PreHandshake,
            daemon: daemon.to_string(),
        }),
    }
}

/// A detected bridge↔daemon version mismatch (ws41-02).
///
/// Produced by [`version_mismatch`] only when the two sides disagree. Carries
/// both versions and renders the direction-aware human message and the
/// pairing key the surfacing dedups on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMismatch {
    /// The bridge's version as it arrived — a reported value or the
    /// pre-handshake sentinel (an older bridge that sent no hello).
    bridge: BridgeVersion,
    /// The version the daemon links.
    daemon: String,
}

/// The bridge side of a [`VersionMismatch`]: a reported version, or the
/// pre-handshake sentinel for a bridge too old to send a hello.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BridgeVersion {
    /// The bridge reported this version in its hello.
    Reported(String),
    /// The bridge sent no hello — an older build predating the handshake.
    PreHandshake,
}

/// The label used for a pre-handshake bridge in messages and the dedup key —
/// an older build that reads as mismatched precisely because it is silent.
const PRE_HANDSHAKE_LABEL: &str = "pre-handshake (unknown)";

impl VersionMismatch {
    /// The bridge version label — the reported value, or the pre-handshake
    /// sentinel for a bridge that sent no hello.
    #[must_use]
    pub fn bridge_label(&self) -> &str {
        match &self.bridge {
            BridgeVersion::Reported(v) => v,
            BridgeVersion::PreHandshake => PRE_HANDSHAKE_LABEL,
        }
    }

    /// The version the daemon links.
    #[must_use]
    pub fn daemon_version(&self) -> &str {
        &self.daemon
    }

    /// A stable key for the once-per-pairing dedup: the observed
    /// `(bridge, daemon)` pairing. Two session-starts reporting the same pair
    /// dedup to one interrupt; a genuinely new pairing fires again.
    #[must_use]
    pub fn pairing_key(&self) -> String {
        format!("{}=>{}", self.bridge_label(), self.daemon)
    }

    /// Whether the bridge is the older side — the case a `/mcp` restart cures.
    ///
    /// A pre-handshake bridge is always older (it predates the handshake). A
    /// reported bridge version is older when it orders below the daemon's under
    /// [`semver_lt`], which compares the dotted numeric components (so
    /// `2.0.9 < 2.0.10`, which a byte-wise compare gets backwards). Only reached
    /// when the two differ, so this decides direction, never equality.
    #[must_use]
    pub fn bridge_is_older(&self) -> bool {
        match &self.bridge {
            BridgeVersion::PreHandshake => true,
            BridgeVersion::Reported(b) => semver_lt(b, &self.daemon),
        }
    }

    /// The direction-aware human message naming the older side and its cure.
    ///
    /// Bridge older → the host holds a stale bridge; the cure is `/mcp` (the
    /// host command that restarts the bridge). Daemon older → the running
    /// daemon links an older protocol than the freshly-spawned bridge; the cure
    /// is to bounce or update the daemon binary. One tier only — every firing is
    /// a true positive by construction, so there is no minor/major split.
    #[must_use]
    pub fn message(&self) -> String {
        let bridge = self.bridge_label();
        let daemon = &self.daemon;
        if self.bridge_is_older() {
            format!(
                "bridge is {bridge}, daemon links {daemon} — the bridge is older; run `/mcp` to restart it"
            )
        } else {
            format!(
                "bridge is {bridge}, daemon links {daemon} — the daemon is older; bounce or update the `catenary` binary"
            )
        }
    }
}

/// Whether `a` orders strictly before `b` as a dotted version.
///
/// Compares the dot-separated components left to right: numeric where both
/// parse (so `2.0.9 < 2.0.10`, which a lexical compare gets wrong), else a
/// byte-wise fallback for a non-numeric component (a pre-release tag). A
/// shorter prefix orders before its longer extension (`2.0 < 2.0.1`). This is a
/// deliberately small ordering — the comparand is the crate's own
/// `CARGO_PKG_VERSION`, a clean `MAJOR.MINOR.PATCH`, and it only picks the
/// *direction* of an already-established inequality, never equality.
fn semver_lt(a: &str, b: &str) -> bool {
    use std::cmp::Ordering;
    let mut ap = a.split('.');
    let mut bp = b.split('.');
    loop {
        match (ap.next(), bp.next()) {
            // Equal so far, or `b` is the shorter prefix — `a` is not before `b`.
            (None | Some(_), None) => return false,
            (None, Some(_)) => return true, // `a` is a shorter prefix → older
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xi), Ok(yi)) => xi.cmp(&yi),
                    _ => x.cmp(y),
                };
                match ord {
                    Ordering::Less => return true,
                    Ordering::Greater => return false,
                    Ordering::Equal => {}
                }
            }
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
    fn matching_versions_are_no_mismatch() {
        assert!(version_mismatch(Some("1.2.3"), "1.2.3").is_none());
    }

    #[test]
    fn semver_lt_orders_numeric_components_not_lexically() {
        assert!(semver_lt("2.0.9", "2.0.10"), "9 < 10 numerically");
        assert!(!semver_lt("2.0.10", "2.0.9"));
        assert!(semver_lt("2.0", "2.0.1"), "a shorter prefix is older");
        assert!(!semver_lt("2.1.0", "2.0.9"));
        assert!(semver_lt("1.9.0", "2.0.0"));
    }

    #[test]
    fn bridge_older_double_digit_patch_names_mcp() {
        // The regression the byte-wise compare had: bridge 2.0.9 vs daemon
        // 2.0.10 must read as bridge-older (cure `/mcp`), not daemon-older.
        let m = version_mismatch(Some("2.0.9"), "2.0.10").expect("mismatch");
        assert!(m.bridge_is_older(), "2.0.9 is older than 2.0.10");
        assert!(m.message().contains("/mcp"));
    }

    #[test]
    fn bridge_older_names_mcp() {
        let m = version_mismatch(Some("1.2.0"), "1.3.0").expect("mismatch");
        assert!(m.bridge_is_older());
        assert!(m.message().contains("/mcp"), "message: {}", m.message());
        assert!(m.message().contains("bridge is older"));
        assert_eq!(m.bridge_label(), "1.2.0");
        assert_eq!(m.daemon_version(), "1.3.0");
    }

    #[test]
    fn daemon_older_names_the_binary() {
        let m = version_mismatch(Some("1.4.0"), "1.3.0").expect("mismatch");
        assert!(!m.bridge_is_older());
        assert!(
            m.message().contains("daemon is older"),
            "message: {}",
            m.message()
        );
        assert!(m.message().contains("catenary` binary"));
        assert!(
            !m.message().contains("/mcp"),
            "daemon-older cure is not /mcp"
        );
    }

    #[test]
    fn pre_handshake_bridge_is_a_mismatch_and_older() {
        let m = version_mismatch(None, "1.3.0").expect("pre-handshake reads as mismatch");
        assert!(m.bridge_is_older(), "a silent bridge is always older");
        assert_eq!(m.bridge_label(), PRE_HANDSHAKE_LABEL);
        assert!(m.message().contains("/mcp"));
    }

    #[test]
    fn pairing_key_is_stable_and_distinguishes_pairs() {
        let a = version_mismatch(Some("1.2.0"), "1.3.0").expect("mismatch");
        let a2 = version_mismatch(Some("1.2.0"), "1.3.0").expect("mismatch");
        let b = version_mismatch(Some("1.1.0"), "1.3.0").expect("mismatch");
        assert_eq!(a.pairing_key(), a2.pairing_key());
        assert_ne!(a.pairing_key(), b.pairing_key());

        let pre = version_mismatch(None, "1.3.0").expect("mismatch");
        assert_ne!(pre.pairing_key(), a.pairing_key());
    }
}
