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

/// One finding per configured server, sorted by name.
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
        .map(|name| {
            let class = classify_server(config, name, feed.active_languages());
            match feed.server_status(name) {
                Some(ServerStatus::Ready { .. }) => {
                    let suffix = ready_suffix(config, name);
                    Finding::new(
                        FindingCode::ServerReady,
                        Severity::Ok,
                        format!("{name}: ready{suffix}"),
                    )
                }
                Some(status) => {
                    let finding = broken_finding(name, class, status);
                    attach_provenance(finding, config, name, feed)
                }
                None => Finding::new(
                    FindingCode::ServerDormant,
                    Severity::Info,
                    format!("{name}: not probed"),
                ),
            }
        })
        .collect()
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
            let _ = client.shutdown().await;
            ServerStatus::Ready { capabilities }
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
/// rustup installs its proxies under `<CARGO_HOME>/bin` (default `~/.cargo/bin`);
/// the real component lives under `~/.rustup/toolchains/<tc>/bin`. A proxy in the
/// cargo-bin dir is the shim that exists regardless of component installation.
fn is_rustup_proxy(resolved: &std::path::Path) -> bool {
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .map(|c| c.join("bin"));
    cargo_bin.is_some_and(|dir| resolved.parent() == Some(dir.as_path()))
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
            },
        );
        let feed = ProbeFeed::new(statuses, HashSet::new(), None);
        let findings = server_findings(&config, &feed);
        let finding = findings.first().expect("one server finding");
        assert_eq!(finding.code, FindingCode::ServerReady);
        assert_eq!(finding.severity, Severity::Ok);
        assert!(finding.message.contains("ready"));
    }

    #[test]
    fn probe_timeout_default_is_five_minutes() {
        assert_eq!(probe_timeout(), Duration::from_mins(5));
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
