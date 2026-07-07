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
use crate::health::servers::{HealthFeed, ServerStatus, binary_exists};
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

        // Routed servers with no live instance: a PATH check distinguishes
        // "not installed" (a problem/suggestion) from "installed but idle".
        for (name, def) in &config.server {
            if statuses.contains_key(name) {
                continue;
            }
            let routed = crate::health::servers::is_routed(config, name, &active_languages);
            if routed && !binary_exists(&def.command) {
                statuses.insert(
                    name.clone(),
                    ServerStatus::BinaryNotFound(def.command.clone()),
                );
            }
        }

        let daemon_version = {
            let v = &snapshot.daemon.version;
            (!v.is_empty()).then(|| v.clone())
        };

        Self {
            statuses,
            active_languages,
            daemon_version,
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
        // Healthy / busy / initializing / probing — up, not broken.
        ServerStatus::Ready {
            capabilities: Vec::new(),
        }
    }
}

/// The active workspace languages implied purely by the live board.
///
/// The language of every non-dead server instance — the cheap half of the
/// routed-vs-dormant input (no filesystem walk). The caller unions it with a
/// cached filesystem detection for the missing-binary/suggestion cases.
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
    if let Some(f) = crate::health::skew::skew_finding(env!("CATENARY_VERSION"), daemon_version) {
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
        | FindingCode::ServerReady => f
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

    fn routed_config(lang: &str, server: &str, command: &str) -> Config {
        let mut config = Config::default();
        config.server.insert(
            server.to_string(),
            ServerDef {
                command: command.to_string(),
                ..ServerDef::default()
            },
        );
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
    fn failed_instance_aggregates_to_broken_even_beside_healthy() {
        let config = routed_config("mylang", "my-ls", "my-ls");
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
        let config = routed_config("mylang", "my-ls", "my-ls-binary-xyz");
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
}
