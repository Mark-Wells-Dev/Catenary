// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The TUI's snapshot feed into the health model, and finding gathering.
//!
//! This is the TUI leg of "one health model, two renderers" (DESIGN): doctor
//! feeds the model with its own one-shot `initialize` probes; the TUI feeds it
//! with the daemon's **live** server states from `state.json` and a PATH-only
//! binary check — it **never probes** (a probing TUI would be a second LSP
//! client fighting the pool) and never opens the firehose.
//!
//! [`SnapshotFeed`] materializes a [`HealthFeed`] from a [`Snapshot`] plus the
//! resolved [`Config`]; [`gather`] runs every model check the TUI can know and
//! tags each finding with the board node that [owns](Owner) it, so the problems
//! pane and the inline tree render two views of the same findings.

use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::health::servers::{HealthFeed, Provenance, ServerStatus, server_binary_installed};
use crate::health::{Finding, FindingCode, Severity};
use crate::state_snapshot::{ServerEntry, Snapshot};

/// The board node a finding belongs to — its inline home and the target a
/// problems-pane selection jumps the board to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A config / daemon-level finding with no single node — the problems pane
    /// is its only home.
    Global,
    /// A configured server (by name) — renders at that server's tree rows and
    /// focuses the root/server tree.
    Server(String),
    /// A host client (by name, e.g. `antigravity`) — renders at the client
    /// node and focuses the client/session tree.
    Client(String),
}

/// A [`Finding`] tagged with the board node that owns it.
#[derive(Debug, Clone)]
pub struct OwnedFinding {
    /// The underlying typed finding.
    pub finding: Finding,
    /// The node this finding renders at / focuses.
    pub owner: Owner,
}

/// The `respawns` count at or above which an up server is judged crash-looping
/// (a Warning — impaired but running).
pub const CRASH_LOOP_THRESHOLD: u32 = 3;

/// Whether a live server-entry state string denotes a terminal failure.
fn is_failed_state(state: &str) -> bool {
    matches!(state, "failed" | "dead")
}

/// Whether a live server-entry state string denotes a healthy, serving state.
fn is_up_state(state: &str) -> bool {
    matches!(state, "healthy" | "busy" | "initializing" | "probing")
}

/// A materialized snapshot feed: server statuses aggregated from the live board
/// (plus a PATH check for routed-but-absent servers), the active languages, and
/// the daemon's version.
pub struct SnapshotFeed {
    statuses: HashMap<String, ServerStatus>,
    active_languages: HashSet<String>,
    daemon_version: Option<String>,
    provenance: HashMap<String, Provenance>,
}

impl SnapshotFeed {
    /// Build the feed from the live snapshot, the resolved config, and the set
    /// of active workspace languages (live instances ∪ detected files).
    ///
    /// Aggregation is **broken-anywhere-is-broken**: a server with any failed
    /// or dead instance reports [`ServerStatus::InitializeFailed`] even if
    /// another instance is healthy, so "is rust-analyzer broken anywhere?" is a
    /// problem (DESIGN). A routed server with no live instance whose binary is
    /// absent on `$PATH` reports [`ServerStatus::BinaryNotFound`] — the only
    /// place the feed touches the filesystem, and never the LSP.
    #[must_use]
    pub fn build(snapshot: &Snapshot, config: &Config, active_languages: HashSet<String>) -> Self {
        let mut statuses: HashMap<String, ServerStatus> = HashMap::new();

        // Aggregate live instances per server name.
        for entry in &snapshot.servers {
            let name = &entry.server;
            let slot = statuses.entry(name.clone());
            match slot {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(status_for_entry(entry));
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    // A failure anywhere wins over a healthy elsewhere.
                    if is_failed_state(&entry.state) && o.get().is_ready() {
                        o.insert(status_for_entry(entry));
                    }
                }
            }
        }

        // Intent-routed servers with no live instance: a PATH check
        // distinguishes "not installed" (a problem/suggestion) from "installed
        // but idle".
        for (name, def) in &config.server {
            if statuses.contains_key(name) {
                continue;
            }
            let routed = crate::health::servers::is_intent_routed(config, name, &active_languages);
            // The server key IS the executable (misc 162); `server_binary_installed`
            // is honest about the rust-analyzer rustup proxy shim.
            let program = def.program(name);
            if routed && !server_binary_installed(name, program) {
                statuses.insert(
                    name.clone(),
                    ServerStatus::BinaryNotFound(program.to_string()),
                );
            }
        }

        let daemon_version = {
            let v = &snapshot.daemon.version;
            (!v.is_empty()).then(|| v.clone())
        };

        // Provenance rides the same activity ledger the gate reads, so the
        // "why is this live?" evidence never disagrees with the finding.
        let (_, provenance) = crate::health::servers::activity_inputs(&snapshot.activity_languages);

        Self {
            statuses,
            active_languages,
            daemon_version,
            provenance,
        }
    }
}

impl HealthFeed for SnapshotFeed {
    fn server_status(&self, server: &str) -> Option<&ServerStatus> {
        self.statuses.get(server)
    }

    fn active_languages(&self) -> &HashSet<String> {
        &self.active_languages
    }

    fn daemon_version(&self) -> Option<&str> {
        self.daemon_version.as_deref()
    }

    fn language_provenance(&self, language: &str) -> Option<&Provenance> {
        self.provenance.get(language)
    }
}

/// Map a single live server entry to a [`ServerStatus`].
fn status_for_entry(entry: &ServerEntry) -> ServerStatus {
    if is_failed_state(&entry.state) {
        let reason = entry
            .last_message
            .as_ref()
            .map(|m| m.text.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("server {}", entry.state));
        ServerStatus::InitializeFailed(reason)
    } else {
        // Healthy / busy / initializing / probing — up, not broken. The
        // snapshot carries no `serverInfo.version`, so the drift input stays
        // `None` here — silence, never a fabricated drift (lsm 03).
        ServerStatus::Ready {
            capabilities: Vec::new(),
            version: None,
        }
    }
}

/// The languages of every up-or-failed server instance on the live board.
///
/// A convenience view of the board's languages. It is **not** the health
/// suggestion/Fatal gate — that is now the activity ledger
/// ([`crate::health::servers::activity_inputs`]), so a server the daemon spawned
/// on mere presence never counts as intent (tui-rework 09, item 5).
#[must_use]
pub fn live_languages(snapshot: &Snapshot) -> HashSet<String> {
    snapshot
        .servers
        .iter()
        .filter(|s| is_up_state(&s.state) || is_failed_state(&s.state))
        .filter(|s| !s.language.is_empty())
        .map(|s| s.language.clone())
        .collect()
}

/// Gather every finding the TUI can know from the snapshot + config.
///
/// Each finding is tagged with its owner. Mirrors doctor's check set (config
/// migration/validation/unknown-key/unreferenced/duplicate, server health,
/// install health, version skew) plus the live-only signals doctor cannot see
/// (027 coverage degradation, crash-looping). `project_root` scopes the
/// project-config / antigravity install checks.
#[must_use]
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
pub fn gather(
    snapshot: &Snapshot,
    config: &Config,
    project_root: &std::path::Path,
    active_languages: HashSet<String>,
) -> Vec<OwnedFinding> {
    let mut out = gather_snapshot(snapshot, config, active_languages);
    out.extend(gather_environment(config, project_root));
    out
}

/// The findings derivable from the snapshot plus the resolved config **alone**.
///
/// No filesystem or `$PATH` reads — version skew (daemon vs this binary),
/// config-struct validation (validation / unreferenced / duplicate-extension),
/// the live server-health aggregation, and the live-only board signals (027
/// degradation, crash-looping).
///
/// Hermetic given its inputs, so this is the seam the fleet-scale render tests
/// drive: a synthetic snapshot + an injected config produce a deterministic
/// finding set independent of the machine's real config files or install state.
#[must_use]
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
pub fn gather_snapshot(
    snapshot: &Snapshot,
    config: &Config,
    active_languages: HashSet<String>,
) -> Vec<OwnedFinding> {
    let mut out: Vec<OwnedFinding> = Vec::new();

    // ── Version skew (daemon vs this binary) ─────────────────────────
    let daemon_version =
        (!snapshot.daemon.version.is_empty()).then_some(snapshot.daemon.version.as_str());
    if let Some(f) =
        crate::health::skew::skew_finding(crate::health::skew::BINARY_VERSION, daemon_version)
    {
        out.push(OwnedFinding {
            finding: f,
            owner: Owner::Global,
        });
    }

    // ── Bridge↔daemon protocol-version mismatch (ws41-02) ────────────
    // The daemon records an observed mismatch onto the snapshot; it persists as
    // a board finding until the versions agree (a `/mcp` restart or a daemon
    // bounce clears the record). Absent record → no finding.
    if let Some(f) = snapshot.daemon.bridge_mismatch.as_ref().and_then(|m| {
        crate::health::skew::bridge_mismatch_finding(m.bridge_version.as_deref(), &m.daemon_version)
    }) {
        out.push(OwnedFinding {
            finding: f,
            owner: Owner::Global,
        });
    }

    // ── Config-struct validation (pure over the resolved config) ─────
    let mut config_findings = crate::health::config_checks::validation_findings(config);
    config_findings.extend(crate::health::config_checks::unreferenced_server_findings(
        config,
    ));
    config_findings.extend(crate::health::config_checks::duplicate_extension_findings(
        config,
    ));
    for f in config_findings {
        out.push(OwnedFinding {
            finding: f,
            owner: Owner::Global,
        });
    }

    // ── Server-health findings (owner = server) ──────────────────────
    let feed = SnapshotFeed::build(snapshot, config, active_languages);
    for f in crate::health::servers::server_findings(config, &feed) {
        // Only surface problems/suggestions from the aggregate; Ok/Info stay
        // out of the finding stream (the tree renders healthy state directly).
        if f.is_problem() || f.severity == Severity::Suggestion {
            let owner = server_name_from_finding(&f).map_or(Owner::Global, Owner::Server);
            out.push(OwnedFinding { finding: f, owner });
        }
    }

    // ── Enrichment-only disclosure (diagnostics-debt 04b) ────────────
    // Each configured, routed server absent from the blessed manifest is
    // enrichment-only — a warn-tier board disclosure naming it, owner = server.
    for f in crate::health::servers::enrichment_only_findings(config, feed.active_languages()) {
        let owner = server_name_from_finding(&f).map_or(Owner::Global, Owner::Server);
        out.push(OwnedFinding { finding: f, owner });
    }

    // ── Live-only server signals from the board ──────────────────────
    out.extend(live_server_findings(snapshot));

    out
}

/// The findings that require reading the machine's environment: the config-file
/// migration / unknown-key walks, the project-config warnings, and the
/// install-health checks (hooks, instructions, `$PATH`). These touch the
/// filesystem and `$PATH`, so they are kept out of the hermetic snapshot seam.
fn gather_environment(config: &Config, project_root: &std::path::Path) -> Vec<OwnedFinding> {
    let mut out: Vec<OwnedFinding> = Vec::new();

    // ── Config-file findings (Global) ────────────────────────────────
    let sources = crate::config::config_sources();
    let mut config_findings = crate::health::config_checks::migration_findings(&sources);
    config_findings.extend(crate::health::config_checks::unknown_key_findings(&sources));
    if crate::health::config_checks::project_config_path(project_root).is_some() {
        config_findings.extend(crate::health::config_checks::project_config_findings(
            project_root,
            config,
        ));
    }
    for f in config_findings {
        out.push(OwnedFinding {
            finding: f,
            owner: Owner::Global,
        });
    }

    // ── Install-health findings (owner = client) ─────────────────────
    out.extend(install_findings(config, project_root));

    out
}

/// The server name a server finding names, parsed from its `"<name>: …"`
/// message prefix (the model's stable message shape).
fn server_name_from_finding(f: &Finding) -> Option<String> {
    match f.code {
        FindingCode::ServerRoutedBroken
        | FindingCode::ServerInstallSuggestion
        | FindingCode::ServerDormant
        | FindingCode::ServerEnrichmentOnly
        | FindingCode::ServerReady
        | FindingCode::ServerVersionDrift => f
            .message
            .split(':')
            .next()
            .map(str::trim)
            .map(str::to_string),
        _ => None,
    }
}

/// Live-only per-server signals the snapshot exposes but a probe cannot:
/// decision-027 coverage degradation and crash-looping.
fn live_server_findings(snapshot: &Snapshot) -> Vec<OwnedFinding> {
    let mut out = Vec::new();
    let mut degraded_seen: HashSet<&str> = HashSet::new();
    let mut looping_seen: HashSet<&str> = HashSet::new();

    for entry in &snapshot.servers {
        if let Some(reason) = &entry.degraded_reason
            && degraded_seen.insert(&entry.server)
        {
            out.push(OwnedFinding {
                finding: Finding::new(
                    FindingCode::ServerRoutedBroken,
                    Severity::Warning,
                    format!("{}: coverage degraded — {reason}", entry.server),
                )
                .with_fix_it(
                    "Re-run `catenary diagnostics` once the server recovers; \
                     coverage restores automatically."
                        .to_string(),
                ),
                owner: Owner::Server(entry.server.clone()),
            });
        }
        if entry.respawns >= CRASH_LOOP_THRESHOLD
            && is_up_state(&entry.state)
            && looping_seen.insert(&entry.server)
        {
            out.push(OwnedFinding {
                finding: Finding::new(
                    FindingCode::ServerRoutedBroken,
                    Severity::Warning,
                    format!(
                        "{}: crash-looping ({} respawns)",
                        entry.server, entry.respawns
                    ),
                )
                .with_fix_it(format!(
                    "Run `catenary doctor {}` for the spawn/initialize transcript.",
                    entry.server
                )),
                owner: Owner::Server(entry.server.clone()),
            });
        }
    }

    // Struck-out (benched) servers — the strike ledger's terminal state
    // (misc 167). Shared derivation with doctor (`health::servers`), so the
    // two renderers never diverge.
    for finding in crate::health::servers::strike_findings(&snapshot.servers) {
        let owner = finding
            .message
            .split(':')
            .next()
            .map(str::trim)
            .map_or(Owner::Global, |name| Owner::Server(name.to_string()));
        out.push(OwnedFinding { finding, owner });
    }

    out
}

/// Install-health findings (hooks / instructions / path / filter), each tagged
/// to the client it belongs to so it renders inline at that client node.
fn install_findings(config: &Config, project_root: &std::path::Path) -> Vec<OwnedFinding> {
    let mut out = Vec::new();

    for f in crate::health::install_checks::claude_hooks_findings() {
        push_install(&mut out, f, "claude");
    }
    for f in crate::health::install_checks::claude_instructions_findings() {
        push_install(&mut out, f, "claude");
    }
    for f in crate::health::install_checks::antigravity_hooks_findings(project_root) {
        push_install(&mut out, f, "antigravity");
    }
    for f in crate::health::install_checks::antigravity_instructions_findings(project_root) {
        push_install(&mut out, f, "antigravity");
    }
    for f in crate::health::install_checks::path_binary_findings() {
        push_install(&mut out, f, "");
    }
    for f in crate::health::install_checks::legacy_script_findings() {
        push_install(&mut out, f, "");
    }
    for f in crate::health::install_checks::command_filter_findings(config) {
        push_install(&mut out, f, "");
    }

    out
}

/// Push an install finding as an [`OwnedFinding`], but only when it is a
/// problem — the confirmed-good `*Ok` findings are silence (DESIGN: nothing
/// green shouts). A blank client name owns to [`Owner::Global`].
fn push_install(out: &mut Vec<OwnedFinding>, finding: Finding, client: &str) {
    if !finding.is_problem() {
        return;
    }
    let owner = if client.is_empty() {
        Owner::Global
    } else {
        Owner::Client(client.to_string())
    };
    out.push(OwnedFinding { finding, owner });
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::{LanguageConfig, ServerBinding, ServerDef};

    fn routed_config(lang: &str, server: &str) -> Config {
        let mut config = Config::default();
        // The key IS the executable (misc 162): a bare def spawns `server`.
        config
            .server
            .insert(server.to_string(), ServerDef::default());
        config.language.insert(
            lang.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server)]),
                ..Default::default()
            },
        );
        config
    }

    fn server_entry(server: &str, language: &str, state: &str) -> ServerEntry {
        ServerEntry {
            id: format!("{server}@/p/root"),
            language: language.to_string(),
            server: server.to_string(),
            scope_root: "/p/root".to_string(),
            state: state.to_string(),
            state_since: crate::state_snapshot::now_iso(),
            ..ServerEntry::default()
        }
    }

    #[test]
    fn bridge_mismatch_on_snapshot_surfaces_a_global_finding() {
        use crate::state_snapshot::{BridgeMismatch, DaemonSnapshot};

        let snap = Snapshot {
            daemon: DaemonSnapshot {
                version: crate::health::skew::BINARY_VERSION.to_string(),
                bridge_mismatch: Some(BridgeMismatch {
                    bridge_version: Some("2.0.1".to_string()),
                    daemon_version: "2.0.2".to_string(),
                }),
                ..DaemonSnapshot::default()
            },
            ..Snapshot::default()
        };
        let findings = gather_snapshot(&snap, &Config::default(), HashSet::new());
        let mismatch = findings
            .iter()
            .find(|f| f.finding.code == crate::health::FindingCode::BridgeVersionMismatch)
            .expect("a recorded mismatch surfaces a board finding");
        assert_eq!(mismatch.owner, Owner::Global);
        assert!(mismatch.finding.message.contains("bridge is older"));
    }

    #[test]
    fn no_bridge_mismatch_record_surfaces_no_finding() {
        // A snapshot with no recorded mismatch (versions agree, or a daemon
        // predating the field) produces no bridge-mismatch finding.
        let snap = Snapshot::default();
        let findings = gather_snapshot(&snap, &Config::default(), HashSet::new());
        assert!(
            !findings
                .iter()
                .any(|f| f.finding.code == crate::health::FindingCode::BridgeVersionMismatch),
            "silence is the healthy state",
        );
    }

    #[test]
    fn failed_instance_aggregates_to_broken_even_beside_healthy() {
        let config = routed_config("mylang", "my-ls");
        let snap = Snapshot {
            servers: vec![
                server_entry("my-ls", "mylang", "healthy"),
                server_entry("my-ls", "mylang", "failed"),
            ],
            ..Snapshot::default()
        };
        let feed = SnapshotFeed::build(&snap, &config, live_languages(&snap));
        assert!(
            matches!(
                feed.server_status("my-ls"),
                Some(ServerStatus::InitializeFailed(_))
            ),
            "broken anywhere is broken",
        );
    }

    #[test]
    fn routed_missing_binary_reports_binary_not_found() {
        let config = routed_config("mylang", "my-ls");
        let snap = Snapshot::default();
        // No live instance, but the language is active (detected).
        let active: HashSet<String> = std::iter::once("mylang".to_string()).collect();
        let feed = SnapshotFeed::build(&snap, &config, active);
        assert!(
            matches!(
                feed.server_status("my-ls"),
                Some(ServerStatus::BinaryNotFound(_))
            ),
            "a routed server with an absent binary is flagged",
        );
    }

    #[test]
    fn crash_loop_surfaces_as_warning() {
        let mut entry = server_entry("rust-analyzer", "rust", "healthy");
        entry.respawns = CRASH_LOOP_THRESHOLD;
        let snap = Snapshot {
            servers: vec![entry],
            ..Snapshot::default()
        };
        let findings = live_server_findings(&snap);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding.severity, Severity::Warning);
        assert!(findings[0].finding.message.contains("crash-looping"));
    }

    #[test]
    fn degradation_surfaces_as_warning_with_fixit() {
        let mut entry = server_entry("gopls", "go", "healthy");
        entry.degraded_reason = Some("first-run timeout".to_string());
        let snap = Snapshot {
            servers: vec![entry],
            ..Snapshot::default()
        };
        let findings = live_server_findings(&snap);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding.severity, Severity::Warning);
        assert!(findings[0].finding.fix_it.is_some());
    }

    #[test]
    fn fixture_language_without_activity_is_silent_touching_it_surfaces_it() {
        // Acceptance (tui-rework 09, item 5): a default server routed to a
        // language present only via a spawned-then-failed instance — the exact
        // conformance-fixture shape — raises NO finding while no session has
        // touched a file of that language. Recording activity for it makes the
        // same failure a Fatal, carrying the triggering file as provenance.
        let config = routed_config("cmake", "cmake-language-server");
        let snap = Snapshot {
            servers: vec![server_entry("cmake-language-server", "cmake", "failed")],
            ..Snapshot::default()
        };

        let quiet = gather_snapshot(&snap, &config, HashSet::new());
        assert!(
            !quiet
                .iter()
                .any(|f| matches!(&f.owner, Owner::Server(s) if s == "cmake-language-server")),
            "a fixture language no session touched raises no finding",
        );

        // `snap` is done being read above, so move it into the touched variant.
        let mut touched = snap;
        touched
            .activity_languages
            .push(crate::state_snapshot::LanguageActivity {
                language: "cmake".to_string(),
                root: "/p/root".to_string(),
                files: vec!["tests/fixtures/conformance/cmake/CMakeLists.txt".to_string()],
                file_count: 1,
            });
        let active: HashSet<String> = std::iter::once("cmake".to_string()).collect();
        let loud = gather_snapshot(&touched, &config, active);
        let cmake = loud
            .iter()
            .find(|f| matches!(&f.owner, Owner::Server(s) if s == "cmake-language-server"))
            .expect("touching a cmake file surfaces the failure");
        assert_eq!(cmake.finding.severity, Severity::Fatal);
        assert!(
            cmake
                .finding
                .provenance
                .as_deref()
                .unwrap_or_default()
                .contains("CMakeLists.txt"),
            "provenance names the triggering file: {:?}",
            cmake.finding.provenance,
        );
    }
}
