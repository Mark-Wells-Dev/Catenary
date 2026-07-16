// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server health: the routed-vs-dormant derivation, the data-feed seam, and the
//! probe feed doctor drives with the daemon down.
//!
//! A server is **routed** iff a configured language binding targets it *and*
//! either that language is **activity-live** (a tracked session touched a file
//! of it) or the server is explicitly configured ([`is_intent_routed`]).
//! Everything else configured is **dormant inventory** — never a warning, even
//! with a missing binary (feedback 08: `csharp-ls: command not found` is noise
//! when nothing routes to it). Activity-gating (tui-rework 09) means presence
//! alone — a vendored sample or a fixture directory no session opened — never
//! lights a language.
//!
//! The [`HealthFeed`] seam supplies the live inputs the model cannot derive from
//! config files alone: each server's observed runtime [`ServerStatus`], the set
//! of activity-live languages and their [`Provenance`], and the daemon's
//! version. Doctor materializes a [`ProbeFeed`] from its own one-shot
//! `initialize` probes plus the snapshot's activity ledger; the TUI materializes
//! a snapshot feed from `state.json`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::config::Config;
use crate::health::{Finding, FindingCode, Severity};
use crate::lsp;

/// Routing provenance for a language (tui-rework 09, items 4–5).
///
/// The root and representative file(s) whose touch made the language
/// activity-live — the "why is this server being probed at all?" evidence,
/// carried on a routed-broken or suggestion [`Finding`] and rendered under its
/// fix-it line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The tracked root the touch happened under, in display form (home path
    /// collapsed to `~`).
    pub root: String,
    /// A representative touched file, root-relative.
    pub file: String,
    /// Total distinct touched files for this language in `root`.
    pub file_count: usize,
}

impl Provenance {
    /// The one-line provenance string, e.g.
    /// `routed by tests/fixtures/conformance/cmake/CMakeLists.txt (1 file) in ~/Projects/Catenary`.
    #[must_use]
    pub fn render(&self) -> String {
        let files = if self.file_count == 1 {
            "1 file".to_string()
        } else {
            format!("{} files", self.file_count)
        };
        format!("routed by {} ({files}) in {}", self.file, self.root)
    }
}

/// Derive the activity-live language set and per-language provenance from a
/// snapshot's activity ledger (tui-rework 09, items 4–5).
///
/// The single source both the TUI snapshot feed and doctor read, so the
/// suggestion/Fatal gate and the provenance never diverge across the two
/// renderers. A language touched under several roots keeps the provenance with
/// the most evidence (highest file count).
#[must_use]
pub fn activity_inputs(
    activity: &[crate::state_snapshot::LanguageActivity],
) -> (HashSet<String>, HashMap<String, Provenance>) {
    let mut languages = HashSet::new();
    let mut provenance: HashMap<String, Provenance> = HashMap::new();
    for entry in activity {
        if entry.language.is_empty() {
            continue;
        }
        languages.insert(entry.language.clone());
        let candidate = Provenance {
            root: display_root(&entry.root),
            file: entry.files.first().cloned().unwrap_or_default(),
            file_count: entry.file_count,
        };
        match provenance.entry(entry.language.clone()) {
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if candidate.file_count > o.get().file_count {
                    o.insert(candidate);
                }
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(candidate);
            }
        }
    }
    (languages, provenance)
}

/// Collapse a canonical root path's home prefix to `~` for provenance display.
fn display_root(root: &str) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = std::path::Path::new(root).strip_prefix(&home)
    {
        if rel.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rel.display());
    }
    root.to_string()
}

/// Observed runtime status of a configured server, from whichever feed supplies
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    /// The server initialized successfully; carries its Catenary capabilities.
    Ready {
        /// Catenary tool names derived from the server's LSP capabilities.
        capabilities: Vec<&'static str>,
        /// The version the server reported in the `initialize` result's
        /// `serverInfo.version` (lsm 03) — the version-drift input. `None`
        /// when the server reported no `serverInfo`/version (silence: absence
        /// of evidence is not drift) or the feed carries no version data (the
        /// TUI snapshot feed).
        version: Option<String>,
    },
    /// The configured binary was not found on `$PATH`.
    BinaryNotFound(String),
    /// The process failed to spawn.
    SpawnFailed(String),
    /// The `initialize` request failed.
    InitializeFailed(String),
    /// The `initialize` request timed out.
    TimedOut,
}

impl ServerStatus {
    /// Whether the server initialized successfully.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// The status text (no leading glyph), e.g. `command not found`.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Ready { .. } => "ready".to_string(),
            Self::BinaryNotFound(cmd) => format!("{cmd}: command not found"),
            Self::SpawnFailed(e) => format!("spawn failed: {e}"),
            Self::InitializeFailed(e) => format!("initialize failed: {e}"),
            Self::TimedOut => "initialize timed out".to_string(),
        }
    }
}

/// Whether a configured server is actually reached by the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerClass {
    /// A language binding targets it and that language is active in a root.
    Routed,
    /// Configured but unreached — dormant inventory.
    Dormant,
}

/// The seam between the health model and its data source.
///
/// Two feeds satisfy it with identical knowledge, differing only in how they
/// gather it: doctor's [`ProbeFeed`] (one-shot probes, daemon-down capable) and
/// the TUI's future snapshot feed (`state.json`). The trait is deliberately
/// synchronous — a feed *materializes* its data first (doctor probes
/// concurrently, the TUI reads the snapshot), then answers these queries over
/// what it gathered.
pub trait HealthFeed {
    /// The observed runtime status of `server`, if the feed has one.
    fn server_status(&self, server: &str) -> Option<&ServerStatus>;
    /// The languages made **activity-live** — a tracked session touched a file
    /// of them (snapshot feed), or doctor read the same activity ledger. The
    /// routed-vs-dormant input, gated on activity rather than presence so a
    /// dormant fixture directory no one touched lights nothing (tui-rework 09,
    /// item 5).
    fn active_languages(&self) -> &HashSet<String>;
    /// The running daemon's version, if known — the version-skew input.
    fn daemon_version(&self) -> Option<&str>;
    /// The routing provenance for `language`, if the feed tracked it — the
    /// "why is this live?" evidence rendered under a finding (item 4). Feeds
    /// without provenance (older snapshots, tests) return `None`.
    fn language_provenance(&self, language: &str) -> Option<&Provenance> {
        let _ = language;
        None
    }
}

/// A materialized probe feed: the statuses doctor's async probes gathered, the
/// activity-live languages and their provenance (read from the snapshot's
/// activity ledger), and the daemon version it observed.
#[derive(Debug, Default)]
pub struct ProbeFeed {
    statuses: HashMap<String, ServerStatus>,
    active_languages: HashSet<String>,
    daemon_version: Option<String>,
    provenance: HashMap<String, Provenance>,
}

impl ProbeFeed {
    /// Build a probe feed from gathered statuses, activity-live languages, and
    /// an optional observed daemon version, with no provenance.
    #[must_use]
    pub fn new(
        statuses: HashMap<String, ServerStatus>,
        active_languages: HashSet<String>,
        daemon_version: Option<String>,
    ) -> Self {
        Self {
            statuses,
            active_languages,
            daemon_version,
            provenance: HashMap::new(),
        }
    }

    /// Attach per-language provenance (from the snapshot activity ledger),
    /// chainable.
    #[must_use]
    #[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
    pub fn with_provenance(mut self, provenance: HashMap<String, Provenance>) -> Self {
        self.provenance = provenance;
        self
    }
}

impl HealthFeed for ProbeFeed {
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

/// Whether `server` is *routed*: a configured `[lsp.language.*]` binding targets
/// it and that language is **activity-live** (present in `active_languages`).
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
#[must_use]
pub fn is_routed(config: &Config, server: &str, active_languages: &HashSet<String>) -> bool {
    config.language.iter().any(|(lang, lc)| {
        active_languages.contains(lang) && lc.servers().iter().any(|b| b.name == server)
    })
}

/// Whether some `[lsp.language.*]` binding targets `server`.
fn has_binding(config: &Config, server: &str) -> bool {
    config
        .language
        .values()
        .any(|lc| lc.servers().iter().any(|b| b.name == server))
}

/// Whether `server` is *explicitly configured* — defined under a non-default
/// name, i.e. a user/project layer named it. Mirrors [`server_intent`]'s config
/// arm: a user def reusing a default name adopts the default's stance.
fn is_explicitly_configured(server: &str) -> bool {
    !crate::config::default_server_names().contains(server)
}

/// Whether `server` is **routed** for health classification (tui-rework 09).
///
/// True when a binding targets it *and* either that language is activity-live
/// ([`is_routed`]) or the server is explicitly configured — the intent axis the
/// severity ladder gates Fatal on. An installed-but-failing *default* server
/// whose language no tracked session touched is not intent: it stays dormant
/// Info inventory, not a Fatal. An explicitly-configured server is intent
/// regardless of activity.
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
#[must_use]
pub fn is_intent_routed(config: &Config, server: &str, active_languages: &HashSet<String>) -> bool {
    is_routed(config, server, active_languages)
        || (is_explicitly_configured(server) && has_binding(config, server))
}

/// Classify `server` as [`ServerClass::Routed`] or [`ServerClass::Dormant`],
/// gated on the intent axis ([`is_intent_routed`]).
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
#[must_use]
pub fn classify_server(
    config: &Config,
    server: &str,
    active_languages: &HashSet<String>,
) -> ServerClass {
    if is_intent_routed(config, server, active_languages) {
        ServerClass::Routed
    } else {
        ServerClass::Dormant
    }
}

/// One finding per configured server, sorted by name — plus, for a ready
/// server whose reported version drifted from its blessed pin, the advisory
/// drift finding (lsm 03).
///
/// The severity is the routed-vs-dormant derivation crossed with the **intent**
/// axis (severity ladder, ratified 2026-07-07):
///
/// - a ready server is [`Severity::Ok`];
/// - a **routed** server that failed its probe *with intent evidence*
///   (explicitly configured, or its binary is installed) is a
///   [`Severity::Fatal`] — you chose it and it isn't working;
/// - a **routed** default binding whose binary is simply absent, with no
///   explicit config, is a [`Severity::Suggestion`] — an unchosen gap ("you
///   have these files; install this server");
/// - a **dormant** server that failed is [`Severity::Info`] inventory — never
///   a problem, however broken its binary.
#[must_use]
pub fn server_findings(config: &Config, feed: &dyn HealthFeed) -> Vec<Finding> {
    let mut names: Vec<&String> = config.server.keys().collect();
    names.sort_unstable();

    names
        .into_iter()
        .flat_map(|name| {
            let class = classify_server(config, name, feed.active_languages());
            match feed.server_status(name) {
                Some(ServerStatus::Ready { version, .. }) => {
                    let suffix = ready_suffix(config, name);
                    let mut findings = vec![Finding::new(
                        FindingCode::ServerReady,
                        Severity::Ok,
                        format!("{name}: ready{suffix}"),
                    )];
                    findings.extend(version_drift_finding(name, version.as_deref()));
                    findings
                }
                Some(status) => {
                    let finding = broken_finding(name, class, status);
                    vec![attach_provenance(finding, config, name, feed)]
                }
                None => vec![Finding::new(
                    FindingCode::ServerDormant,
                    Severity::Info,
                    format!("{name}: not probed"),
                )],
            }
        })
        .collect()
}

/// The advisory version-drift finding for a ready server, or `None` (lsm 03).
///
/// Consults the active blessed manifest
/// ([`crate::recipes::BlessedManifest::version_drift`]): silence for an
/// unreported version, a matching version, an unblessed server, or the
/// exempt toolchain-tracking rust-analyzer. Detection only — never a spawn
/// gate, never a problem severity.
fn version_drift_finding(name: &str, running: Option<&str>) -> Option<Finding> {
    let running = running?;
    let drift = crate::recipes::active_manifest().version_drift(name, running)?;
    Some(drift_finding(name, &drift))
}

/// Render a [`crate::recipes::VersionDrift`] into its doctor finding: the
/// message names both versions (pinned and running); the fix-it carries the
/// warranty-tier annotation on the row's evidence class.
///
/// [`Severity::Info`], deliberately: running your own server version is a
/// choice, not a fault — the finding discloses the warranty scope honestly and
/// changes nothing.
fn drift_finding(name: &str, drift: &crate::recipes::VersionDrift) -> Finding {
    Finding::new(
        FindingCode::ServerVersionDrift,
        Severity::Info,
        format!(
            "{name}: running version {} differs from the blessed {} ({})",
            drift.running, drift.pinned, drift.platform
        ),
    )
    .with_fix_it(format!(
        "{}. Running your own version is a choice, not a fault — nothing is \
         refused; the conformance warranty simply does not cover it.",
        drift.warranty_annotation(name)
    ))
}

/// Attach the routing provenance (item 4) to a routed-broken or suggestion
/// finding: the first bound language the feed tracked provenance for. Other
/// findings pass through untouched — provenance answers "why is this server
/// being probed?", which only makes sense for a routed server.
fn attach_provenance(
    finding: Finding,
    config: &Config,
    server: &str,
    feed: &dyn HealthFeed,
) -> Finding {
    if !matches!(
        finding.code,
        FindingCode::ServerRoutedBroken | FindingCode::ServerInstallSuggestion
    ) {
        return finding;
    }
    match routing_provenance(config, server, feed) {
        Some(provenance) => finding.with_provenance(provenance),
        None => finding,
    }
}

/// The rendered provenance for the first `[lsp.language.*]` binding to `server`
/// the feed has provenance for, or `None`.
fn routing_provenance(config: &Config, server: &str, feed: &dyn HealthFeed) -> Option<String> {
    config
        .language
        .iter()
        .filter(|(_, lc)| lc.servers().iter().any(|b| b.name == server))
        .find_map(|(lang, _)| feed.language_provenance(lang))
        .map(Provenance::render)
}

/// Whether a broken routed server carries **intent** — the axis that splits a
/// [`Severity::Fatal`] (you chose it) from a [`Severity::Suggestion`] (an
/// unchosen default gap).
///
/// Two evidences, either sufficient:
/// - **installation** — the binary is present (any status other than
///   [`ServerStatus::BinaryNotFound`] means the process spawned or tried to);
/// - **explicit config** — a user or project layer named the server, i.e. it
///   is not a pure embedded default. Mirrors the model's existing provenance
///   stance ([`crate::config::default_server_names`]): a user def reusing a
///   default name adopts the default's exemption.
fn server_intent(name: &str, status: &ServerStatus) -> bool {
    if !matches!(status, ServerStatus::BinaryNotFound(_)) {
        return true;
    }
    !crate::config::default_server_names().contains(name)
}

/// Build the `file_patterns: [...]` suffix for a ready server's message.
fn ready_suffix(config: &Config, server: &str) -> String {
    let patterns = config
        .server
        .get(server)
        .map(|def| def.file_patterns.as_slice())
        .unwrap_or_default();
    if patterns.is_empty() {
        String::new()
    } else {
        format!(
            "  file_patterns: [{}]",
            patterns
                .iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Build the finding for a server that failed its probe, split by class and —
/// for a routed break — by intent ([`server_intent`]).
fn broken_finding(name: &str, class: ServerClass, status: &ServerStatus) -> Finding {
    let detail = status.detail();
    match class {
        ServerClass::Routed if server_intent(name, status) => {
            // Fatal — an intent-broken server you chose or installed.
            let finding = Finding::new(
                FindingCode::ServerRoutedBroken,
                Severity::Fatal,
                format!("{name}: {detail}"),
            );
            match status {
                ServerStatus::BinaryNotFound(cmd) => finding.with_fix_it(format!(
                    "`{cmd}` is configured but wasn't found on your `$PATH`. \
                     Install it or correct the path."
                )),
                _ => finding.with_fix_it(format!(
                    "Run `catenary doctor {name}` for the full spawn/initialize transcript."
                )),
            }
        }
        ServerClass::Routed => {
            // Suggestion — a live language with a default binding but no binary
            // and no explicit config. Name the binary; never fabricate an
            // install command (pinned recipes are a later ticket).
            let binary = match status {
                ServerStatus::BinaryNotFound(cmd) => cmd.as_str(),
                _ => name,
            };
            Finding::new(
                FindingCode::ServerInstallSuggestion,
                Severity::Suggestion,
                format!("{name}: not installed"),
            )
            .with_fix_it(format!(
                "Install `{binary}` to enable {name} coverage. \
                 Catenary never runs installs for you."
            ))
        }
        ServerClass::Dormant => Finding::new(
            FindingCode::ServerDormant,
            Severity::Info,
            format!("{name}: {detail} (dormant — nothing routes here)"),
        ),
    }
}

/// Strike-ledger findings from the live server board (misc 167): one
/// [`Severity::Warning`] per struck-out (benched) server, deduplicated by
/// server name.
///
/// The shared derivation both renderers read — doctor from the `state.json`
/// snapshot it already opens for the activity ledger, the TUI from its
/// snapshot feed — so "struck out — needs attention" never diverges across
/// the two. A benched server is impaired-but-recoverable state (the bug-79
/// escape keeps sessions unblocked), hence Warning, not Fatal: demand revives
/// are suspended until the daemon restarts or the root remounts.
#[must_use]
pub fn strike_findings(servers: &[crate::state_snapshot::ServerEntry]) -> Vec<Finding> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut findings = Vec::new();
    for entry in servers {
        let Some(cause) = entry.benched.as_deref() else {
            continue;
        };
        if !seen.insert(entry.server.as_str()) {
            continue;
        }
        let detail = if cause == "never started" {
            "never started (spawn/initialize failed repeatedly)"
        } else {
            "gave up after repeated crashes"
        };
        findings.push(
            Finding::new(
                FindingCode::ServerRoutedBroken,
                Severity::Warning,
                format!("{}: struck out — {detail}; revives suspended", entry.server),
            )
            .with_fix_it(format!(
                "Run `catenary doctor {}` for the spawn/initialize transcript. \
                 Restart the daemon (`catenary stop`, then `catenary start`) or \
                 remount the root to re-arm revives.",
                entry.server,
            )),
        );
    }
    findings
}

/// Auto-install findings from the live daemon board (lsm 05): the
/// doctor-visible half of every background auto-install.
///
/// A `failed` record is the ruled skip-with-finding — one
/// [`Severity::Warning`] naming the reason, with the retry semantics in the
/// fix-it (no retry loop; the next session start's detection retries
/// naturally). An `installing`/`installed` record renders as non-problem
/// [`Severity::Info`] inventory, so every auto-install leaves a doctor-visible
/// trace (the announcement floor). Records live exactly as long as the daemon;
/// with the daemon down there is no board and no findings.
#[must_use]
pub fn auto_install_findings(entries: &[crate::state_snapshot::AutoInstallEntry]) -> Vec<Finding> {
    entries
        .iter()
        .map(|entry| match entry.status.as_str() {
            "failed" => Finding::new(
                FindingCode::ServerAutoInstallFailed,
                Severity::Warning,
                format!(
                    "{}: auto-install of {} failed — {}",
                    entry.server,
                    entry.version,
                    entry.detail.as_deref().unwrap_or("no reason recorded"),
                ),
            )
            .with_fix_it(
                "Auto-install does not retry on its own; the next session start's detection \
                 retries naturally. Check network access, or install through the guided \
                 install (`catenary`, problems pane)."
                    .to_string(),
            ),
            "installed" => Finding::new(
                FindingCode::ServerAutoInstall,
                Severity::Info,
                format!(
                    "{}: auto-installed {} into the managed home",
                    entry.server, entry.version,
                ),
            ),
            _ => Finding::new(
                FindingCode::ServerAutoInstall,
                Severity::Info,
                format!(
                    "{}: auto-install of {} in progress",
                    entry.server, entry.version,
                ),
            ),
        })
        .collect()
}

/// One warn-tier finding per configured, routed **unverified** server
/// (diagnostics-debt 04b / DESIGN §"The blessed set").
///
/// An unverified server is a custom `[lsp.server.*]` def absent from the blessed
/// manifest, so it is enrichment-only and never a diagnostics source.
///
/// The loud, user-facing disclosure the design requires: an unverified server
/// still enriches grep/glob, but Catenary withholds its diagnostics (no
/// capability advertised, publishes ignored, the gate never arms). The finding
/// names the server and points at the blessed manifest so the user knows *why*
/// and *where* to look — declaration, never a Catenary-quirk apology.
///
/// Scoped to **intent-routed** servers ([`is_intent_routed`]): a dormant
/// unverified def nothing routes to is inventory, not an actionable disclosure —
/// consistent with the routed-vs-dormant severity ladder. A blessed server (the
/// common case) never produces a finding here.
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
#[must_use]
pub fn enrichment_only_findings(
    config: &Config,
    active_languages: &HashSet<String>,
) -> Vec<Finding> {
    let mut names: Vec<&String> = config.server.keys().collect();
    names.sort_unstable();

    names
        .into_iter()
        .filter(|name| {
            !crate::recipes::is_server_blessed(name)
                && is_intent_routed(config, name, active_languages)
        })
        .map(|name| {
            Finding::new(
                FindingCode::ServerEnrichmentOnly,
                Severity::Warning,
                format!(
                    "{name}: unverified custom server — enrichment-only, not a diagnostics source"
                ),
            )
            .with_fix_it(
                "This server is not in Catenary's blessed manifest, so its diagnostics are \
                 withheld: it advertises no diagnostics capability, its publishes are ignored, \
                 and the edit gate never arms for its files. It still enriches grep/glob. Only a \
                 server that passed the CI conformance gate is trusted to report diagnostics — \
                 see `defaults/blessed-manifest.toml`.",
            )
        })
        .collect()
}

/// Default per-server timeout for the initialize probe (5 minutes).
///
/// Julia's `LanguageServer.jl` compiles on first run and can take minutes
/// without a precompiled sysimage. Override with `CATENARY_DOCTOR_TIMEOUT_SECS`.
fn probe_timeout() -> Duration {
    std::env::var("CATENARY_DOCTOR_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or_else(|| Duration::from_mins(5), Duration::from_secs)
}

/// Probe a single server: binary check → spawn → initialize → capabilities →
/// shutdown. Returns the server name paired with its observed [`ServerStatus`].
///
/// This is doctor's own one-shot probe — daemon-down capable, since it spawns
/// the server directly rather than asking a running daemon.
///
/// `program` is the executable the server key resolves to (misc 162): the
/// server's own `path` override if set, else the key itself on `PATH`.
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
pub async fn probe_server(
    name: String,
    program: String,
    args: Vec<String>,
    initialization_options: Option<serde_json::Value>,
    env: Option<HashMap<String, String>>,
) -> (String, ServerStatus) {
    if !server_binary_installed(&name, &program) {
        return (name, ServerStatus::BinaryNotFound(program));
    }

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let spawn_result = lsp::LspClient::spawn_quiet(
        &program,
        &args_refs,
        &name,
        &name,
        crate::logging::LoggingServer::new(),
        env.as_ref(),
    );

    let mut client = match spawn_result {
        Ok(client) => client,
        Err(e) => return (name, ServerStatus::SpawnFailed(e.to_string())),
    };

    let init_result = tokio::time::timeout(
        probe_timeout(),
        client.initialize(&[], initialization_options),
    )
    .await;

    let status = match init_result {
        Ok(Ok(result)) => {
            let capabilities =
                extract_capabilities(&result["capabilities"], client.supports_type_hierarchy());
            // The drift input (lsm 03): the `serverInfo.version` the client
            // captured from the initialize result, nearly free at probe time.
            let version = client.server_version().map(str::to_string);
            let _ = client.shutdown().await;
            ServerStatus::Ready {
                capabilities,
                version,
            }
        }
        Ok(Err(e)) => {
            let _ = client.shutdown().await;
            ServerStatus::InitializeFailed(e.to_string())
        }
        Err(_) => {
            let _ = client.shutdown().await;
            ServerStatus::TimedOut
        }
    };
    (name, status)
}

/// Whether a binary can be found on `$PATH`.
#[must_use]
pub fn binary_exists(command: &str) -> bool {
    resolve_binary(command).is_some()
}

/// Whether the executable for server `name` (resolving to `program`) is actually
/// **installed** — honest against the rust-analyzer rustup proxy shim (misc 162).
///
/// For every server but rust-analyzer this is a plain `$PATH` resolution
/// ([`binary_exists`]). rust-analyzer is special: since misc 162 the server key
/// is the `rust-analyzer` rustup *proxy* on `PATH`, which is present the moment
/// rustup is installed — **even when the `rust-analyzer` component is not** (the
/// class-A shim). Trusting the shim's existence would report a Fatal
/// intent-broken server (you "installed" it, it fails) where the honest reading
/// is an uninstalled default (a Suggestion). So when `program` resolves through
/// the key to a rustup proxy, we cross-check that the component itself resolves —
/// the same signal the conformance workflow relies on when it links the real
/// component ahead of the shim. A relocating `path` override bypasses all of this
/// (the user pointed us at a concrete binary).
#[must_use]
pub fn server_binary_installed(name: &str, program: &str) -> bool {
    let Some(resolved) = resolve_binary(program) else {
        return false;
    };
    // Only the rust-analyzer *key* (not a `path` override) can hit the proxy shim.
    if name == "rust-analyzer" && program == "rust-analyzer" && is_rustup_proxy(&resolved) {
        return rustup_component_installed("rust-analyzer");
    }
    true
}

/// Whether `resolved` is a rustup proxy shim rather than a real component binary.
///
/// A rustup proxy is not identified by *where* it sits — distro-packaged rustup
/// keeps its shims outside `<CARGO_HOME>/bin` (Arch: `/usr/lib/rustup/bin`), so a
/// directory test alone silently misses every distro layout (bug 92). Instead we
/// identify the proxy by what it **is**: rustup proxies are hardlinks (or copies)
/// of the `rustup` binary itself. We resolve the `rustup` binary on `PATH` and
/// treat `resolved` as a proxy when it is the *same file* —
///
/// - the same inode (identical `dev`+`ino`), which catches hardlinks and the same
///   path reached two ways; or
/// - the same target after symlink resolution ([`std::fs::canonicalize`]), which
///   catches a symlinked shim.
///
/// A copy that is byte-identical but a distinct inode is deliberately **not**
/// identity here (nothing ties the two files together on disk); such a layout
/// falls through to the known-directory heuristic below (`<CARGO_HOME>/bin`,
/// default `~/.cargo/bin`), which still catches the classic cargo-bin copy.
fn is_rustup_proxy(resolved: &std::path::Path) -> bool {
    if let Some(rustup) = resolve_binary("rustup")
        && same_file(resolved, &rustup)
    {
        return true;
    }
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .map(|c| c.join("bin"));
    cargo_bin.is_some_and(|dir| resolved.parent() == Some(dir.as_path()))
}

/// Whether `a` and `b` are the *same file* on disk: the same inode (identical
/// `dev`+`ino`, catching hardlinks) or the same canonical target after symlink
/// resolution. `MetadataExt` is safe under `forbid(unsafe_code)`.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    if let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b))
        && ma.dev() == mb.dev()
        && ma.ino() == mb.ino()
    {
        return true;
    }
    matches!(
        (std::fs::canonicalize(a), std::fs::canonicalize(b)),
        (Ok(ca), Ok(cb)) if ca == cb
    )
}

/// Whether the rustup `component` resolves to a real binary in the active
/// toolchain — the shim-proof "is the component actually installed?" check.
///
/// `rustup which <component>` resolves through the active toolchain and prints the
/// component's path; it **fails** (non-zero, or an error to stderr) when the
/// component is not installed. We then confirm the printed path exists on disk, so
/// a stale/bogus resolution cannot pass. This is the doctor half of the
/// conformance workflow's `rustup which` step (misc 162) — a real probe, not a
/// `which()` the shim would defeat.
fn rustup_component_installed(component: &str) -> bool {
    let Ok(output) = std::process::Command::new("rustup")
        .args(["which", component])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();
    !path.is_empty() && std::path::Path::new(path).is_file()
}

/// Resolve a binary command to its full path on `$PATH`, or `None`.
#[must_use]
pub fn resolve_binary(command: &str) -> Option<std::path::PathBuf> {
    if command.contains('/') {
        let p = std::path::PathBuf::from(command);
        return p.exists().then_some(p);
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(command))
        .find(|p| p.is_file())
}

/// Extract Catenary tool names from LSP server capabilities.
#[must_use]
pub fn extract_capabilities(caps: &serde_json::Value, type_hierarchy: bool) -> Vec<&'static str> {
    let has = |key: &str| caps.get(key).is_some_and(|v| !v.is_null());

    let mut tools = Vec::new();
    if has("hoverProvider") {
        tools.push("hover");
    }
    if has("definitionProvider") {
        tools.push("definition");
    }
    if has("typeDefinitionProvider") {
        tools.push("type_definition");
    }
    if has("implementationProvider") {
        tools.push("implementation");
    }
    if has("referencesProvider") {
        tools.push("references");
    }
    if has("documentSymbolProvider") {
        tools.push("document_symbols");
    }
    if has("workspaceSymbolProvider") {
        tools.push("search");
    }
    if has("codeActionProvider") {
        tools.push("code_actions");
    }
    if has("callHierarchyProvider") {
        tools.push("call_hierarchy");
    }
    if type_hierarchy {
        tools.push("type_hierarchy");
    }
    tools
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::{LanguageConfig, ServerBinding, ServerDef};

    /// Build a config with one language routing to one server.
    fn routed_config(lang: &str, server: &str) -> Config {
        let mut config = Config::default();
        config
            .server
            .insert(server.to_string(), ServerDef::default());
        let lang_config = LanguageConfig {
            servers: Some(vec![ServerBinding::new(server)]),
            ..Default::default()
        };
        config.language.insert(lang.to_string(), lang_config);
        config
    }

    #[test]
    fn enrichment_only_finding_names_the_unverified_routed_server() {
        // diagnostics-debt 04b: a configured, routed unverified server (a custom
        // def absent from the blessed manifest) earns exactly one warn-tier
        // disclosure naming it enrichment-only, with the manifest as the pointer.
        let config = routed_config("custlang", "my-custom-server");
        // An explicitly-configured server is intent-routed on its binding alone.
        let findings = enrichment_only_findings(&config, &HashSet::new());
        let finding = findings
            .iter()
            .find(|f| f.code == FindingCode::ServerEnrichmentOnly)
            .expect("one enrichment-only finding for the unverified server");
        assert_eq!(finding.severity, Severity::Warning, "warn-tier disclosure");
        assert!(
            finding.message.contains("my-custom-server")
                && finding.message.contains("enrichment-only"),
            "the finding names the server and declares it enrichment-only: {}",
            finding.message,
        );
        assert!(
            finding
                .fix_it
                .as_deref()
                .is_some_and(|f| f.contains("blessed-manifest.toml")),
            "the fix-it points at the manifest",
        );
    }

    #[test]
    fn enrichment_only_finding_silent_for_blessed_server() {
        // A blessed server (rust-analyzer) is a diagnostics source, so it produces
        // NO enrichment-only finding — the disclosure fires only for the unverified.
        let config = routed_config("rust", "rust-analyzer");
        let active: HashSet<String> = std::iter::once("rust".to_string()).collect();
        let findings = enrichment_only_findings(&config, &active);
        assert!(
            findings.is_empty(),
            "a blessed server is never enrichment-only: {findings:?}",
        );
    }

    #[test]
    fn routed_requires_binding_and_active_language() {
        let config = routed_config("rust", "rust-analyzer");
        let active: HashSet<String> = std::iter::once("rust".to_string()).collect();
        assert!(
            is_routed(&config, "rust-analyzer", &active),
            "a bound + active language routes to the server",
        );
        // Binding exists but the language is not active in any root → dormant.
        assert!(
            !is_routed(&config, "rust-analyzer", &HashSet::new()),
            "no active language means dormant, even with a binding",
        );
        // Active language but no binding to this server → not routed.
        assert!(
            !is_routed(&config, "some-other-server", &active),
            "an unbound server is never routed",
        );
    }

    #[test]
    fn dormant_missing_binary_is_info_not_error() {
        // The feedback-08 behavior change: a dormant server whose binary is
        // missing must be inventory (Info), never an error.
        let config = routed_config("rust", "rust-analyzer");
        let mut statuses = HashMap::new();
        statuses.insert(
            "rust-analyzer".to_string(),
            ServerStatus::BinaryNotFound("rust-analyzer".to_string()),
        );
        // No active languages → the server is dormant.
        let feed = ProbeFeed::new(statuses, HashSet::new(), None);
        let findings = server_findings(&config, &feed);
        let finding = findings.first().expect("one server finding");
        assert_eq!(finding.code, FindingCode::ServerDormant);
        assert_eq!(finding.severity, Severity::Info);
        assert!(
            finding.message.contains("command not found"),
            "the status text is preserved: {}",
            finding.message,
        );
        assert!(
            finding.message.contains("dormant"),
            "a dormant server is labelled: {}",
            finding.message,
        );
    }

    #[test]
    fn routed_missing_binary_of_configured_server_is_fatal() {
        // A user-named server (not an embedded default) with a missing binary
        // is intent-broken: you explicitly configured it, so a PATH break
        // eating it ranks with a crash — Fatal.
        let config = routed_config("mylang", "my-custom-ls");
        let mut statuses = HashMap::new();
        statuses.insert(
            "my-custom-ls".to_string(),
            ServerStatus::BinaryNotFound("my-custom-ls".to_string()),
        );
        let active: HashSet<String> = std::iter::once("mylang".to_string()).collect();
        let feed = ProbeFeed::new(statuses, active, None);
        let findings = server_findings(&config, &feed);
        let finding = findings.first().expect("one server finding");
        assert_eq!(finding.code, FindingCode::ServerRoutedBroken);
        assert_eq!(finding.severity, Severity::Fatal);
        assert!(finding.fix_it.is_some(), "a fatal break carries fix-it");
    }

    #[test]
    fn routed_missing_binary_of_default_is_suggestion() {
        // A shipped default binding (rust-analyzer) with no binary and no
        // explicit config is an unchosen gap — a Suggestion, never a problem.
        let config = routed_config("rust", "rust-analyzer");
        let mut statuses = HashMap::new();
        statuses.insert(
            "rust-analyzer".to_string(),
            ServerStatus::BinaryNotFound("rust-analyzer".to_string()),
        );
        let active: HashSet<String> = std::iter::once("rust".to_string()).collect();
        let feed = ProbeFeed::new(statuses, active, None);
        let findings = server_findings(&config, &feed);
        let finding = findings.first().expect("one server finding");
        assert_eq!(finding.code, FindingCode::ServerInstallSuggestion);
        assert_eq!(finding.severity, Severity::Suggestion);
        assert!(
            !finding.severity.is_problem(),
            "a suggestion is not a problem"
        );
        let fix_it = finding
            .fix_it
            .as_deref()
            .expect("suggestion names a binary");
        assert!(
            fix_it.contains("rust-analyzer"),
            "names the binary: {fix_it}"
        );
        assert!(
            !fix_it.contains("cargo") && !fix_it.contains("npm") && !fix_it.contains("curl"),
            "never fabricates an install command: {fix_it}",
        );
    }

    #[test]
    fn routed_installed_but_init_failed_is_fatal() {
        // A default-named server whose binary IS present but init failed carries
        // intent via installation — Fatal, not a suggestion.
        let config = routed_config("rust", "rust-analyzer");
        let mut statuses = HashMap::new();
        statuses.insert(
            "rust-analyzer".to_string(),
            ServerStatus::InitializeFailed("boom".to_string()),
        );
        let active: HashSet<String> = std::iter::once("rust".to_string()).collect();
        let feed = ProbeFeed::new(statuses, active, None);
        let findings = server_findings(&config, &feed);
        let finding = findings.first().expect("one server finding");
        assert_eq!(finding.code, FindingCode::ServerRoutedBroken);
        assert_eq!(finding.severity, Severity::Fatal);
    }

    #[test]
    fn ready_server_is_ok() {
        let config = routed_config("rust", "rust-analyzer");
        let mut statuses = HashMap::new();
        statuses.insert(
            "rust-analyzer".to_string(),
            ServerStatus::Ready {
                capabilities: vec!["hover"],
                version: None,
            },
        );
        let feed = ProbeFeed::new(statuses, HashSet::new(), None);
        let findings = server_findings(&config, &feed);
        let finding = findings.first().expect("one server finding");
        assert_eq!(finding.code, FindingCode::ServerReady);
        assert_eq!(finding.severity, Severity::Ok);
        assert!(finding.message.contains("ready"));
    }

    // ── version drift (lsm 03) ──────────────────────────────────────

    /// A probe feed whose single server probed ready reporting `version`.
    fn ready_feed(server: &str, version: Option<&str>) -> ProbeFeed {
        let mut statuses = HashMap::new();
        statuses.insert(
            server.to_string(),
            ServerStatus::Ready {
                capabilities: Vec::new(),
                version: version.map(str::to_string),
            },
        );
        ProbeFeed::new(statuses, HashSet::new(), None)
    }

    #[test]
    fn ready_drifted_version_earns_the_advisory_drift_finding() {
        // taplo is blessed at 0.10.0 in the shipped seed manifest; a ready
        // probe reporting a different version earns the advisory finding —
        // naming both versions — alongside (never replacing) the Ok.
        let config = routed_config("toml", "taplo");
        let findings = server_findings(&config, &ready_feed("taplo", Some("0.11.0")));

        assert_eq!(findings.len(), 2, "ready + drift: {findings:?}");
        assert_eq!(findings[0].code, FindingCode::ServerReady);
        assert_eq!(findings[0].severity, Severity::Ok, "ready stays Ok");

        let drift = &findings[1];
        assert_eq!(drift.code, FindingCode::ServerVersionDrift);
        assert_eq!(drift.severity, Severity::Info, "advisory, never a problem");
        assert!(
            !drift.is_problem(),
            "a version choice never dents a verdict"
        );
        assert!(
            drift.message.contains("0.11.0") && drift.message.contains("0.10.0"),
            "the finding names both versions: {}",
            drift.message
        );
        let fix_it = drift.fix_it.as_deref().expect("carries the annotation");
        assert!(
            fix_it.contains("verified-on-"),
            "the annotation names the row's evidence class: {fix_it}"
        );
        assert!(
            fix_it.contains("choice, not a fault"),
            "the annotation stays advisory: {fix_it}"
        );
    }

    #[test]
    fn ready_matching_version_is_silent() {
        // The blessed version (modulo normalization looseness) → nothing.
        let config = routed_config("toml", "taplo");
        for reported in ["0.10.0", "v0.10.0", " 0.10.0+abc123 "] {
            let findings = server_findings(&config, &ready_feed("taplo", Some(reported)));
            assert_eq!(
                findings.len(),
                1,
                "a vetted version draws no drift for {reported:?}: {findings:?}"
            );
            assert_eq!(findings[0].code, FindingCode::ServerReady);
        }
    }

    #[test]
    fn ready_unreported_version_is_silent() {
        // No `serverInfo`/version → absence of evidence is not drift.
        let config = routed_config("toml", "taplo");
        let findings = server_findings(&config, &ready_feed("taplo", None));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, FindingCode::ServerReady);
    }

    #[test]
    fn ready_rust_analyzer_never_draws_drift() {
        // The toolchain-tracking exemption: whatever version RA reports, no
        // drift finding — its version rides `rustup update` by design.
        let config = routed_config("rust", "rust-analyzer");
        let findings = server_findings(
            &config,
            &ready_feed("rust-analyzer", Some("1.96.0 (abcdef0 2026-07-01)")),
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, FindingCode::ServerReady);
    }

    #[test]
    fn ready_unblessed_server_never_draws_drift() {
        // A custom def absent from the manifest has no pin to drift from
        // (its disclosure is the enrichment-only finding, not drift).
        let config = routed_config("foo", "my-custom-ls");
        let findings = server_findings(&config, &ready_feed("my-custom-ls", Some("9.9.9")));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, FindingCode::ServerReady);
    }

    #[test]
    fn probe_timeout_default_is_five_minutes() {
        assert_eq!(probe_timeout(), Duration::from_mins(5));
    }

    #[test]
    fn strike_findings_warn_per_benched_server_deduped() {
        // misc 167: one Warning per struck-out server (deduplicated across
        // roots), the cause distinguishing broken-config from instability;
        // healthy or merely-striking servers produce nothing.
        let entry = |server: &str, benched: Option<&str>| crate::state_snapshot::ServerEntry {
            id: format!("{server}@/p"),
            server: server.to_string(),
            benched: benched.map(str::to_string),
            ..Default::default()
        };
        let servers = vec![
            entry("ra", Some("never started")),
            entry("ra", Some("never started")), // a second root: deduped
            entry("gopls", Some("unstable")),
            entry("lua-ls", None), // not benched: silent
        ];
        let findings = strike_findings(&servers);
        assert_eq!(findings.len(), 2, "one finding per benched server");

        let ra = &findings[0];
        assert_eq!(ra.code, FindingCode::ServerRoutedBroken);
        assert_eq!(ra.severity, Severity::Warning);
        assert!(ra.message.starts_with("ra: struck out"), "{}", ra.message);
        assert!(ra.message.contains("never started"), "{}", ra.message);

        let gopls = &findings[1];
        assert!(
            gopls.message.contains("gave up after repeated crashes"),
            "{}",
            gopls.message,
        );
        assert!(
            gopls
                .fix_it
                .as_deref()
                .expect("a bench carries a fix-it")
                .contains("catenary doctor gopls"),
        );
    }

    #[test]
    fn extract_capabilities_maps_providers() {
        let caps = serde_json::json!({"hoverProvider": true, "definitionProvider": true});
        let result = extract_capabilities(&caps, false);
        assert!(result.contains(&"hover"));
        assert!(result.contains(&"definition"));
    }

    #[test]
    fn extract_capabilities_ignores_null_values() {
        let caps = serde_json::json!({"hoverProvider": null});
        assert!(!extract_capabilities(&caps, false).contains(&"hover"));
    }

    #[test]
    fn binary_exists_finds_and_rejects() {
        assert!(binary_exists("sh"));
        assert!(!binary_exists("catenary_nonexistent_binary_xyz"));
    }

    #[test]
    fn auto_install_findings_warn_on_failure_and_note_the_rest() {
        // lsm 05: a failed record is the skip-with-finding Warning naming the
        // reason; installing/installed records are non-problem Info inventory
        // (the doctor-visible announcement floor).
        let entry = |status: &str, detail: Option<&str>| crate::state_snapshot::AutoInstallEntry {
            server: "gopls".to_string(),
            version: "v0.20.0".to_string(),
            status: status.to_string(),
            detail: detail.map(str::to_string),
            at: "2026-07-16T00:00:00.000Z".to_string(),
        };

        let failed = auto_install_findings(&[entry("failed", Some("registry unreachable"))]);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].code, FindingCode::ServerAutoInstallFailed);
        assert_eq!(failed[0].severity, Severity::Warning);
        assert!(
            failed[0].message.contains("registry unreachable"),
            "the finding names the reason: {}",
            failed[0].message,
        );
        assert!(
            failed[0]
                .fix_it
                .as_deref()
                .is_some_and(|f| f.contains("next session start")),
            "the fix-it states the natural-retry semantics",
        );

        let rest = auto_install_findings(&[entry("installing", None), entry("installed", None)]);
        assert_eq!(rest.len(), 2);
        assert!(
            rest.iter().all(|f| f.severity == Severity::Info
                && f.code == FindingCode::ServerAutoInstall
                && !f.is_problem()),
            "non-failure records are Info inventory: {rest:?}",
        );
    }

    #[test]
    fn server_binary_installed_matches_path_for_ordinary_servers() {
        // For every server but rust-analyzer, "installed" is a plain PATH check.
        assert!(server_binary_installed("gopls", "sh")); // `sh` resolves
        assert!(!server_binary_installed(
            "gopls",
            "catenary_nonexistent_binary_xyz"
        ));
    }

    #[test]
    fn server_binary_installed_absent_binary_is_not_installed() {
        // A missing binary is never "installed", rust-analyzer or otherwise — the
        // shim distinction only matters once *something* resolves on PATH.
        assert!(!server_binary_installed(
            "rust-analyzer",
            "catenary_nonexistent_binary_xyz"
        ));
    }

    #[test]
    fn server_binary_installed_path_override_bypasses_the_shim_check() {
        // A rust-analyzer server with a concrete `path` override that resolves is
        // installed — the proxy-shim cross-check applies only to the bare key.
        // `sh` stands in for a real, resolvable absolute binary; it is not under
        // ~/.cargo/bin, so `is_rustup_proxy` is false and no rustup probe runs.
        let sh = resolve_binary("sh").expect("sh resolves on the test host");
        let sh = sh.to_string_lossy().into_owned();
        assert!(server_binary_installed("rust-analyzer", &sh));
    }

    #[test]
    fn is_rustup_proxy_identifies_cargo_bin_only() {
        // Compute the cargo-bin dir the same way `is_rustup_proxy` does (reading
        // CARGO_HOME / ~/.cargo), without mutating the process env (the crate
        // forbids `unsafe`, and env is process-global). A binary inside that dir
        // is the proxy shim; one under a toolchain bin (or anywhere else) is not.
        let cargo_bin = std::env::var_os("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
            .map(|c| c.join("bin"))
            .expect("a cargo-bin dir resolves on the test host");

        assert!(
            super::is_rustup_proxy(&cargo_bin.join("rust-analyzer")),
            "a binary under <CARGO_HOME>/bin is the proxy shim",
        );
        assert!(
            !super::is_rustup_proxy(std::path::Path::new(
                "/home/user/.rustup/toolchains/stable/bin/rust-analyzer"
            )),
            "a toolchain-bin component is not the proxy shim",
        );
        assert!(
            !super::is_rustup_proxy(std::path::Path::new("/usr/local/bin/rust-analyzer")),
            "a system-bin binary is not the proxy shim",
        );
    }

    #[test]
    fn same_file_detects_hardlink() {
        // A rustup proxy is a hardlink of the rustup binary: same inode, distinct
        // path. This is the distro-layout case (bug 92) — the proxy sits in an
        // arbitrary dir, not under <CARGO_HOME>/bin.
        let dir = tempfile::tempdir().expect("tempdir");
        let rustup = dir.path().join("rustup");
        std::fs::write(&rustup, b"fake rustup binary").expect("write rustup");
        let proxy = dir.path().join("bin").join("rust-analyzer");
        std::fs::create_dir_all(proxy.parent().expect("parent")).expect("mkdir");
        std::fs::hard_link(&rustup, &proxy).expect("hardlink");

        assert!(
            super::same_file(&proxy, &rustup),
            "a hardlink of the rustup binary is the same file",
        );
    }

    #[test]
    fn same_file_detects_symlink() {
        // A symlinked proxy resolves to the same canonical target as rustup.
        let dir = tempfile::tempdir().expect("tempdir");
        let rustup = dir.path().join("rustup");
        std::fs::write(&rustup, b"fake rustup binary").expect("write rustup");
        let proxy = dir.path().join("bin").join("rust-analyzer");
        std::fs::create_dir_all(proxy.parent().expect("parent")).expect("mkdir");
        std::os::unix::fs::symlink(&rustup, &proxy).expect("symlink");

        assert!(
            super::same_file(&proxy, &rustup),
            "a symlink to the rustup binary resolves to the same file",
        );
    }

    #[test]
    fn same_file_rejects_content_copy() {
        // A byte-identical *copy* is a distinct inode with no on-disk link to
        // rustup — deliberately NOT identity. Such a layout falls back to the
        // known-directory heuristic in `is_rustup_proxy`, not this check.
        let dir = tempfile::tempdir().expect("tempdir");
        let rustup = dir.path().join("rustup");
        std::fs::write(&rustup, b"fake rustup binary").expect("write rustup");
        let copy = dir.path().join("bin").join("rust-analyzer");
        std::fs::create_dir_all(copy.parent().expect("parent")).expect("mkdir");
        std::fs::copy(&rustup, &copy).expect("copy");

        assert!(
            !super::same_file(&copy, &rustup),
            "a same-content copy is a distinct file, not identity",
        );
    }

    #[test]
    fn same_file_rejects_unrelated_binaries() {
        // Two independent files are never the same file.
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("rustup");
        let b = dir.path().join("rust-analyzer");
        std::fs::write(&a, b"one").expect("write a");
        std::fs::write(&b, b"two").expect("write b");

        assert!(
            !super::same_file(&a, &b),
            "unrelated binaries are not the same file",
        );
    }

    #[test]
    fn default_server_broken_without_activity_is_dormant_info() {
        // A shipped-default server (`cmake-language-server`) whose probe fails but whose
        // language no session touched is quiet dormant Info — the phantom Fatal
        // the conformance fixtures produced (tui-rework 09, item 5).
        let config = routed_config("cmake", "cmake-language-server");
        let mut statuses = HashMap::new();
        statuses.insert(
            "cmake-language-server".to_string(),
            ServerStatus::InitializeFailed("boom".to_string()),
        );
        let feed = ProbeFeed::new(statuses, HashSet::new(), None);
        let finding = server_findings(&config, &feed)
            .into_iter()
            .next()
            .expect("one server finding");
        assert_eq!(finding.code, FindingCode::ServerDormant);
        assert_eq!(finding.severity, Severity::Info);
        assert!(
            !finding.severity.is_problem(),
            "no phantom Fatal without activity",
        );
    }

    #[test]
    fn default_server_broken_with_activity_is_fatal() {
        // The same failure, but a tracked session touched a cmake file → the
        // language is activity-live → the failure is an intent-broken Fatal.
        let config = routed_config("cmake", "cmake-language-server");
        let mut statuses = HashMap::new();
        statuses.insert(
            "cmake-language-server".to_string(),
            ServerStatus::InitializeFailed("boom".to_string()),
        );
        let active: HashSet<String> = std::iter::once("cmake".to_string()).collect();
        let feed = ProbeFeed::new(statuses, active, None);
        let finding = server_findings(&config, &feed)
            .into_iter()
            .next()
            .expect("finding");
        assert_eq!(finding.code, FindingCode::ServerRoutedBroken);
        assert_eq!(finding.severity, Severity::Fatal);
    }

    #[test]
    fn explicitly_configured_server_is_intent_routed_without_activity() {
        // A user-named server (non-default name) is intent regardless of
        // activity; a default-named one needs the language to be activity-live.
        let user = routed_config("mylang", "my-custom-ls");
        assert!(is_intent_routed(&user, "my-custom-ls", &HashSet::new()));
        let default_cfg = routed_config("cmake", "cmake-language-server");
        assert!(!is_intent_routed(
            &default_cfg,
            "cmake-language-server",
            &HashSet::new()
        ));
    }

    #[test]
    fn routed_broken_finding_carries_provenance() {
        let config = routed_config("mylang", "my-custom-ls");
        let mut statuses = HashMap::new();
        statuses.insert(
            "my-custom-ls".to_string(),
            ServerStatus::InitializeFailed("boom".to_string()),
        );
        let active: HashSet<String> = std::iter::once("mylang".to_string()).collect();
        let mut provenance = HashMap::new();
        provenance.insert(
            "mylang".to_string(),
            Provenance {
                root: "~/Projects/Catenary".to_string(),
                file: "src/main.ml".to_string(),
                file_count: 2,
            },
        );
        let feed = ProbeFeed::new(statuses, active, None).with_provenance(provenance);
        let finding = server_findings(&config, &feed)
            .into_iter()
            .next()
            .expect("finding");
        let prov = finding.provenance.expect("provenance attached");
        assert!(prov.contains("routed by src/main.ml"), "{prov}");
        assert!(prov.contains("2 files"), "{prov}");
        assert!(prov.contains("~/Projects/Catenary"), "{prov}");
    }

    #[test]
    fn activity_inputs_derives_languages_and_provenance() {
        let activity = vec![crate::state_snapshot::LanguageActivity {
            language: "cmake".to_string(),
            // A non-home path stays verbatim, so the render is deterministic.
            root: "/p/root".to_string(),
            files: vec!["tests/fixtures/CMakeLists.txt".to_string()],
            file_count: 1,
        }];
        let (langs, prov) = activity_inputs(&activity);
        assert!(langs.contains("cmake"));
        let p = prov.get("cmake").expect("provenance for cmake");
        assert_eq!(
            p.render(),
            "routed by tests/fixtures/CMakeLists.txt (1 file) in /p/root",
        );
    }
}
