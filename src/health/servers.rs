// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server health: the routed-vs-dormant derivation, the data-feed seam, and the
//! probe feed doctor drives with the daemon down.
//!
//! A server is **routed** iff a configured language binding targets it *and*
//! that language is active in a tracked root ([`is_routed`]). Everything else
//! configured is **dormant inventory** — never a warning, even with a missing
//! binary (feedback 08: `csharp-ls: command not found` is noise when nothing
//! routes to it). This is the one intended behavior change of the extraction.
//!
//! The [`HealthFeed`] seam supplies the live inputs the model cannot derive from
//! config files alone: each server's observed runtime [`ServerStatus`], the set
//! of active languages (detected files, or a live per-root instance), and the
//! daemon's version. Doctor materializes a [`ProbeFeed`] from its own one-shot
//! `initialize` probes; the TUI (a later phase) will materialize a snapshot feed
//! from `state.json`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::config::Config;
use crate::health::{Finding, FindingCode, Severity};
use crate::lsp;

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
    /// The languages active in a tracked root — detected files, or (for a
    /// snapshot feed) a live per-root instance. The routed-vs-dormant input.
    fn active_languages(&self) -> &HashSet<String>;
    /// The running daemon's version, if known — the version-skew input.
    fn daemon_version(&self) -> Option<&str>;
}

/// A materialized probe feed: the statuses doctor's async probes gathered, the
/// languages it detected in the workspace, and the daemon version it observed.
#[derive(Debug, Default)]
pub struct ProbeFeed {
    statuses: HashMap<String, ServerStatus>,
    active_languages: HashSet<String>,
    daemon_version: Option<String>,
}

impl ProbeFeed {
    /// Build a probe feed from gathered statuses, detected languages, and an
    /// optional observed daemon version.
    #[must_use]
    pub const fn new(
        statuses: HashMap<String, ServerStatus>,
        active_languages: HashSet<String>,
        daemon_version: Option<String>,
    ) -> Self {
        Self {
            statuses,
            active_languages,
            daemon_version,
        }
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
}

/// Whether `server` is *routed*: a configured `[lsp.language.*]` binding targets
/// it and that language is present in `active_languages`.
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
#[must_use]
pub fn is_routed(config: &Config, server: &str, active_languages: &HashSet<String>) -> bool {
    config.language.iter().any(|(lang, lc)| {
        active_languages.contains(lang) && lc.servers().iter().any(|b| b.name == server)
    })
}

/// Classify `server` as [`ServerClass::Routed`] or [`ServerClass::Dormant`].
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
#[must_use]
pub fn classify_server(
    config: &Config,
    server: &str,
    active_languages: &HashSet<String>,
) -> ServerClass {
    if is_routed(config, server, active_languages) {
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
                Some(status) => broken_finding(name, class, status),
                None => Finding::new(
                    FindingCode::ServerDormant,
                    Severity::Info,
                    format!("{name}: not probed"),
                ),
            }
        })
        .collect()
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
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
pub async fn probe_server(
    name: String,
    command: String,
    args: Vec<String>,
    initialization_options: Option<serde_json::Value>,
    env: Option<HashMap<String, String>>,
) -> (String, ServerStatus) {
    if !binary_exists(&command) {
        return (name, ServerStatus::BinaryNotFound(command));
    }

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let spawn_result = lsp::LspClient::spawn_quiet(
        &command,
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
}
